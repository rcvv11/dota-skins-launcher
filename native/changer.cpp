// changer.dll: swaps Dota 2 resource files in memory (the launcher's memory mode).
//
// Hook on CAsyncResourceDataRequest::InitForFileRead (resourcesystem.dll): the resource's
// name stays the same (it stays in the preload manifest and loads as usual), only the
// file its data is read from is swapped (crystal_maiden_cape.vmdl_c ->
// lumini_polare_back.vmdl_c).
//
// The swap table is written by the launcher: %APPDATA%\dota-skins\model_swap.cfg
// (re-read when it changes, applies to resources loaded after that)
//   <original>|<replacement>  paths without the _c suffix
//   hide|<folder>/          every .vmdl of the folder -> an invisible model
//   <path>|keep             an exception to hide
// Log: %APPDATA%\dota-skins\model_swap.log
//
// Build (MinGW): g++ -O2 -shared -static -o changer.dll changer.cpp -lpsapi

#include <windows.h>
#include <psapi.h>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <cstdarg>
#include <string>
#include <vector>
#include <set>
#include <map>
#include <fstream>
#include <unordered_map>
#include <algorithm>

#pragma comment(lib, "psapi.lib")

static std::string g_appDir;
static FILE* g_log = nullptr;
static CRITICAL_SECTION g_logLock;

static void Log(const char* fmt, ...) {
    if (!g_log) return;
    char buf[4096];
    va_list args;
    va_start(args, fmt);
    vsnprintf(buf, sizeof(buf), fmt, args);
    va_end(args);
    EnterCriticalSection(&g_logLock);
    fprintf(g_log, "%llu %s\n", (unsigned long long)GetTickCount64(), buf);
    fflush(g_log);
    LeaveCriticalSection(&g_logLock);
}

// --- CBufferString (tier0) ---
// +0: uint32 length (bits 0..29), +4: uint32 allocated (bits 0..29) | 0x40000000 = data inline at +8
// +8: char* or the inline buffer
static const char* BufStr_Get(void* s) {
    uint32_t alloc = *(uint32_t*)((char*)s + 4);
    if (alloc & 0x40000000) return (const char*)s + 8;
    if ((alloc & 0x3FFFFFFF) == 0) return "";
    return *(const char**)((char*)s + 8);
}

using PurgeFn = void (*)(void* self, int allocatedSize);
static PurgeFn g_Purge = nullptr;

// CBufferString with a built-in 200-byte buffer, as the engine itself creates it
struct StackBufferString {
    uint32_t length = 0;
    uint32_t alloc = 0xC00000C8;
    char data[200 + 8] = {};
    ~StackBufferString() { g_Purge(this, 0); }
};

// --- Swap table ---
// The launcher rewrites the config on the fly: the table is built anew and replaced
// as a whole under an exclusive lock; the hook reads it under a shared one.
struct Rules {
    std::unordered_map<std::string, std::string> replacements;
    std::vector<std::string> hiddenPrefixes;
    std::set<std::string> keep;
};
static Rules* g_rules = new Rules();
static std::string g_lang = "ru";  // language of the interface buttons
static SRWLOCK g_rulesLock = SRWLOCK_INIT;
static const char* kHiddenModel = "models/development/invisiblebox.vmdl";

static std::string Normalize(std::string p) {
    std::transform(p.begin(), p.end(), p.begin(), [](unsigned char c) { return (char)tolower(c); });
    std::replace(p.begin(), p.end(), '\\', '/');
    while (!p.empty() && (p.back() == ' ' || p.back() == '\r' || p.back() == '\t')) p.pop_back();
    while (!p.empty() && (p.front() == ' ' || p.front() == '\t')) p.erase(p.begin());
    return p;
}

static void AddRule(Rules& rules, const std::string& from, const std::string& to) {
    std::string f = Normalize(from), t = Normalize(to);
    if (f.empty() || t.empty() || f == t) return;
    if (f == "hide") {
        if (t.back() != '/') t += '/';
        rules.hiddenPrefixes.push_back(t);
    } else if (t == "keep") {
        rules.keep.insert(f);
    } else {
        rules.replacements.emplace(f, t);  // the first swap of a file wins, as in the file mode
    }
}

static void QueuePreloads(const Rules& rules, const std::set<std::string>& extra);
// Item glows for a match (lines glow|npc|particle|cp:type:attachment;...):
// a JS array the interface script gets through DSLSetGlows
static std::string g_glowsJs = "[]";
// Item models the glows hang on (the 5th field of glow lines), with every model the
// swap table turns into one of them: model in the game -> the item model
static std::unordered_map<std::string, std::string> g_itemModels;
// Items for empty slots the running Dota has not read yet (line
// restart|name\tname): their button says «after restart»
static std::string g_restartJs = "[]";
// Slot tile icons (lines slotdef|HERO|CAPTION|item|style): the preset's item shown
// in the hero page's slot tiles; for the script {HERO: {CAPTION: [item, style]}}
static std::string g_slotsJs = "{}";
// Taunts (lines taunt|npc_dota_hero_x|sequence;sequence): played on your own hero
// in a match, client-side only
static std::map<std::string, std::string> g_taunts;
static SRWLOCK g_tauntLock = SRWLOCK_INIT;
static volatile LONG g_glowsVersion = 0;
static SRWLOCK g_glowsLock = SRWLOCK_INIT;

static std::string JsString(const std::string& v) {
    std::string out = "\"";
    for (char c : v) {
        if (c == '"' || c == '\\') out += '\\';
        if ((unsigned char)c >= 0x20) out += c;
    }
    return out + "\"";
}

static std::vector<std::string> Split(const std::string& s, char sep) {
    std::vector<std::string> out;
    size_t start = 0;
    for (;;) {
        size_t at = s.find(sep, start);
        out.push_back(s.substr(start, at == std::string::npos ? std::string::npos : at - start));
        if (at == std::string::npos) return out;
        start = at + 1;
    }
}

// glow|npc|particle|cp:type:attachment;...|item model|part,part ->
// {npc:"",fx:"",cps:[[cp,"type","att"],...],item:"",parts:["",...]}
// (the effect hangs on the item model when it is found, else its parts on the hero)
static std::string GlowJs(const std::string& line) {
    auto f = Split(line, '|');
    if (f.size() < 4) return std::string();
    std::string cps;
    for (auto& d : Split(f[3], ';')) {
        auto p = Split(d, ':');
        if (p.size() < 3 || p[0].empty()) continue;
        if (!cps.empty()) cps += ",";
        cps += "[" + std::to_string(atoi(p[0].c_str())) + "," + JsString(p[1]) + "," + JsString(p[2]) + "]";
    }
    std::string js = "{npc:" + JsString(f[1]) + ",fx:" + JsString(f[2]) + ",cps:[" + cps + "]";
    if (f.size() > 4 && !f[4].empty()) js += ",item:" + JsString(Normalize(f[4]));
    if (f.size() > 5) {
        std::string parts;
        for (auto& p : Split(f[5], ','))
            if (!p.empty()) parts += (parts.empty() ? "" : ",") + JsString(p);
        js += ",parts:[" + parts + "]";
    }
    return js + "}";
}
static volatile LONG g_refreshPending = 0;  // the config changed: refresh the preview after preloading

static bool LoadConfig(const std::string& path) {
    std::ifstream file(path);
    if (!file.is_open()) return false;
    Rules* fresh = new Rules();
    std::string line, glows, restart;
    std::map<std::string, std::string> taunts;
    std::map<std::string, std::string> slots;  // HERO -> "CAPTION":[item,style],...
    std::set<std::string> itemKeys;            // item models the glows hang on
    std::set<std::string> glowFiles;           // the glows' effects (preloaded)
    std::string lang = "ru";
    while (std::getline(file, line)) {
        if (!line.empty() && line.back() == '\r') line.pop_back();
        if (line.empty() || line[0] == '#') continue;
        if (line.compare(0, 5, "lang|") == 0) { lang = line.substr(5); continue; }
        if (line.compare(0, 8, "slotdef|") == 0) {
            // slotdef|HERO|CAPTION|item|style
            std::vector<std::string> f;
            size_t s = 8;
            while (true) {
                size_t e = line.find('|', s);
                f.push_back(line.substr(s, e == std::string::npos ? std::string::npos : e - s));
                if (e == std::string::npos) break;
                s = e + 1;
            }
            if (f.size() >= 3) {
                // slotdef|HERO|CAPTION|item|style|icon|rarity -> CAPTION: [item, style, icon, rarity]
                std::string& entry = slots[f[0]];
                entry += (entry.empty() ? "" : ",") + JsString(f[1]) + ":[" + std::to_string(atoi(f[2].c_str())) + "," +
                         std::to_string(f.size() > 3 ? atoi(f[3].c_str()) : 0) + "," +
                         JsString(f.size() > 4 ? f[4] : std::string()) + "," +
                         JsString(f.size() > 5 ? f[5] : std::string()) + "]";
            }
            continue;
        }
        if (line.compare(0, 6, "taunt|") == 0) {
            size_t bar = line.find('|', 6);
            if (bar != std::string::npos) taunts[line.substr(6, bar - 6)] = line.substr(bar + 1);
            continue;
        }
        if (line.compare(0, 8, "restart|") == 0) {
            size_t s = 8;
            while (s <= line.size()) {
                size_t e = line.find('\t', s);
                if (e == std::string::npos) e = line.size();
                if (e > s) restart += (restart.empty() ? "" : ",") + JsString(line.substr(s, e - s));
                s = e + 1;
            }
            continue;
        }
        if (line.compare(0, 5, "glow|") == 0) {
            std::string g = GlowJs(line);
            if (!g.empty()) glows += (glows.empty() ? "" : ",") + g;
            auto f = Split(line, '|');
            if (f.size() > 4 && !f[4].empty()) itemKeys.insert(Normalize(f[4]));
            // the effects the script creates itself, loaded in the menu beforehand: a
            // regular match loads nothing on demand (red crosses instead of them)
            if (f.size() > 2 && !f[2].empty()) glowFiles.insert(Normalize(f[2]));
            if (f.size() > 5)
                for (auto& part : Split(f[5], ','))
                    if (!part.empty()) glowFiles.insert(Normalize(part));
            continue;
        }
        size_t pos = line.find('|');
        if (pos != std::string::npos) AddRule(*fresh, line.substr(0, pos), line.substr(pos + 1));
    }
    // an item model is worn under any model the swap table turns into it (the
    // slot's default one, the player's own item)
    std::unordered_map<std::string, std::string> itemModels;
    for (auto& k : itemKeys) itemModels[k] = k;
    for (auto& r : fresh->replacements)
        if (itemKeys.count(r.second)) itemModels.emplace(r.first, r.second);
    AcquireSRWLockExclusive(&g_rulesLock);
    g_lang = (lang == "en") ? "en" : "ru";
    Rules* old = g_rules;
    g_rules = fresh;
    ReleaseSRWLockExclusive(&g_rulesLock);
    AcquireSRWLockExclusive(&g_glowsLock);
    g_glowsJs = "[" + glows + "]";
    g_itemModels.swap(itemModels);
    g_restartJs = "[" + restart + "]";
    {
        std::string js = "{";
        for (auto& s : slots) js += (js.size() > 1 ? "," : "") + JsString(s.first) + ":{" + s.second + "}";
        g_slotsJs = js + "}";
    }
    ReleaseSRWLockExclusive(&g_glowsLock);
    AcquireSRWLockExclusive(&g_tauntLock);
    g_taunts.swap(taunts);
    ReleaseSRWLockExclusive(&g_tauntLock);
    InterlockedIncrement(&g_glowsVersion);
    static bool first = true;
    if (!first) InterlockedExchange(&g_refreshPending, 1);
    first = false;
    delete old;
    QueuePreloads(*fresh, glowFiles);
    Log("[+] Config %s: %zu replacements, %zu hidden folders",
        path.c_str(), fresh->replacements.size(), fresh->hiddenPrefixes.size());
    return true;
}

static bool FileTime(const std::string& path, FILETIME& out) {
    WIN32_FILE_ATTRIBUTE_DATA data;
    if (!GetFileAttributesExA(path.c_str(), GetFileExInfoStandard, &data)) return false;
    out = data.ftLastWriteTime;
    return true;
}

// The launcher rewrites the config when the preset changes: re-read it
static DWORD WINAPI WatchConfig(LPVOID) {
    std::string cfg = g_appDir + "model_swap.cfg";
    FILETIME last{};
    FileTime(cfg, last);
    for (;;) {
        Sleep(50);
        FILETIME now;
        if (FileTime(cfg, now) && CompareFileTime(&now, &last) != 0) {
            last = now;
            Sleep(10);  // the launcher writes through a temp file and a rename
            LoadConfig(cfg);
        }
    }
}

// --- InitForFileRead hook ---
// void InitForFileRead(this, const ResourceBindingBase_t* binding, priority, bool, const char* fileName)
// fileName is usually nullptr: then the engine gets the name itself through ResourceSystem->vtable[0x180/8]
using InitForFileReadFn = void (*)(void* self, void* binding, uintptr_t priority, uintptr_t flag, const char* fileName);
static InitForFileReadFn g_OrigInitForFileRead = nullptr;

using GetFileNameFn = void (*)(void* rs, void* binding, void* outName, uintptr_t zero);
static void* g_ResourceSystem = nullptr;

static bool FindReplacementFile(const Rules& rules, const std::string& base, bool compiled, std::string& out) {
    auto it = rules.replacements.find(base);
    if (it != rules.replacements.end()) {
        out = compiled ? it->second + "_c" : it->second;
        return true;
    }

    const std::string ext = ".vmdl";
    if (base.size() > ext.size() && base.compare(base.size() - ext.size(), ext.size(), ext) == 0 &&
        !rules.keep.count(base)) {
        for (const auto& prefix : rules.hiddenPrefixes) {
            if (base.compare(0, prefix.size(), prefix) == 0) {
                out = std::string(kHiddenModel) + (compiled ? "_c" : "");
                return true;
            }
        }
    }
    return false;
}

static bool FindReplacementFile(const char* fileName, std::string& out) {
    std::string name = Normalize(fileName);
    bool compiled = name.size() > 2 && name.compare(name.size() - 2, 2, "_c") == 0;
    std::string base = compiled ? name.substr(0, name.size() - 2) : name;
    AcquireSRWLockShared(&g_rulesLock);
    bool found = FindReplacementFile(*g_rules, base, compiled, out);
    ReleaseSRWLockShared(&g_rulesLock);
    return found;
}

static void HookedInitForFileRead(void* self, void* binding, uintptr_t priority, uintptr_t flag, const char* fileName) {
    std::string replacement;
    if (fileName && fileName[0]) {
        if (FindReplacementFile(fileName, replacement)) {
            Log("[+] %s -> %s", fileName, replacement.c_str());
            return g_OrigInitForFileRead(self, binding, priority, flag, replacement.c_str());
        }
    } else {
        StackBufferString name;
        auto getName = (*(GetFileNameFn**)g_ResourceSystem)[0x180 / 8];
        getName(g_ResourceSystem, binding, &name, 0);
        const char* cur = BufStr_Get(&name);
        if (cur[0] && FindReplacementFile(cur, replacement)) {
            Log("[+] %s -> %s", cur, replacement.c_str());
            return g_OrigInitForFileRead(self, binding, priority, flag, replacement.c_str());
        }
    }
    g_OrigInitForFileRead(self, binding, priority, flag, fileName);
}

// --- Preloading replacements for the menu ---
// The engine's resource name: a CBufferString with a 200-byte buffer, then a hash and a type.
static volatile LONG g_inMenu = 0;  // the interface script reports: menu, not a match
static volatile LONG g_substituted = 0;     // how many times a replacement was handed out instead of the original

struct ResourceName {
    uint32_t length = 0;
    uint32_t alloc = 0xC00000C8;
    char data[200] = {};
    uint64_t hash = 0;
    uint64_t type = 0;
    ~ResourceName() { g_Purge(this, 0); }
};
static_assert(sizeof(ResourceName) == 0xE0, "ResourceName layout");

using SetResourceNameFn = bool (*)(ResourceName* name, const char* path);
using BlockingLoadFn = void* (*)(void* rs, ResourceName* name, uintptr_t manifest);
using GetStatusFn = int (*)(void* rs, ResourceName* name);
static SetResourceNameFn g_SetResourceName = nullptr;
static BlockingLoadFn g_BlockingLoad = nullptr;
static const int kResourceResident = 3;
static const size_t kGetStatusSlot = 0x198 / 8;

static std::set<std::string> g_preloadQueue;  // replacements still to load
static std::set<std::string> g_resident;      // replacements already loaded: their name can be handed out
static SRWLOCK g_preloadLock = SRWLOCK_INIT;

// Resources handed out as their replacement by name in the menu (bypassing the cache): models and effects
static bool IsSwappableResource(const char* name) {
    size_t len = strlen(name);
    return len > 5 && (_stricmp(name + len - 5, ".vmdl") == 0 || _stricmp(name + len - 5, ".vpcf") == 0 ||
                       _stricmp(name + len - 5, ".vmat") == 0);
}

static void QueuePreloads(const Rules& rules, const std::set<std::string>& extra) {
    AcquireSRWLockExclusive(&g_preloadLock);
    for (auto& r : rules.replacements) {
        const std::string& t = r.second;
        if (IsSwappableResource(t.c_str()) && !g_resident.count(t)) g_preloadQueue.insert(t);
    }
    for (auto& t : extra)
        if (IsSwappableResource(t.c_str()) && !g_resident.count(t)) g_preloadQueue.insert(t);
    ReleaseSRWLockExclusive(&g_preloadLock);
}

static bool PreloadsDone() {
    AcquireSRWLockShared(&g_preloadLock);
    bool r = g_preloadQueue.empty();
    ReleaseSRWLockShared(&g_preloadLock);
    return r;
}

// Models and materials are loaded (effects may still be loading): the hero model
// can be rebuilt already, the effects come with a second refresh
static bool ModelsDone() {
    AcquireSRWLockShared(&g_preloadLock);
    bool r = true;
    for (auto& m : g_preloadQueue) {
        size_t n = m.size();
        if (n > 5 && (_stricmp(m.c_str() + n - 5, ".vmdl") == 0 || _stricmp(m.c_str() + n - 5, ".vmat") == 0)) { r = false; break; }
    }
    ReleaseSRWLockShared(&g_preloadLock);
    return r;
}

static bool IsResident(const std::string& model) {
    AcquireSRWLockShared(&g_preloadLock);
    bool r = g_resident.count(model) != 0;
    ReleaseSRWLockShared(&g_preloadLock);
    return r;
}

// Interface thread (script tick): loads a few replacements per tick, synchronously
static void RunPreloads(ULONGLONG budget = 30) {
    if (!g_SetResourceName || !g_BlockingLoad || !g_ResourceSystem) return;
    // as many as fit in ~30 ms of a tick (a tick every 50 ms): a set loads in a fraction of a second
    // (less in a match: a blocking load there is a hitch)
    ULONGLONG start = GetTickCount64();
    while (GetTickCount64() - start < budget) {
        std::string model;
        AcquireSRWLockExclusive(&g_preloadLock);
        if (!g_preloadQueue.empty()) {
            model = *g_preloadQueue.begin();
            g_preloadQueue.erase(g_preloadQueue.begin());
        }
        ReleaseSRWLockExclusive(&g_preloadLock);
        if (model.empty()) return;

        ResourceName name;
        if (!g_SetResourceName(&name, model.c_str())) continue;
        auto status = (GetStatusFn)(*(void***)g_ResourceSystem)[kGetStatusSlot];
        if (status(g_ResourceSystem, &name) != kResourceResident) g_BlockingLoad(g_ResourceSystem, &name, 0);
        bool ok = status(g_ResourceSystem, &name) == kResourceResident;
        Log("[%c] Preload %s", ok ? '+' : '-', model.c_str());
        if (ok) {
            AcquireSRWLockExclusive(&g_preloadLock);
            g_resident.insert(model);
            ReleaseSRWLockExclusive(&g_preloadLock);
        }
    }
}


// --- Swap on resource lookup (menu) ---
// CResourceSystem::FindOrRegisterResourceByName_Internal(rs, ResourceName*, bool):
// gives a resource's binding by name and hash. In the menu, for an original whose
// replacement is loaded, the replacement's binding is given: as when an item is changed
// with Dota's «Equip», the panel gets another resource and redraws the model.
using FindOrRegisterFn = void* (*)(void* rs, ResourceName* name, uintptr_t flag);
static FindOrRegisterFn g_OrigFindOrRegister = nullptr;

static void* HookedFindOrRegister(void* rs, ResourceName* name, uintptr_t flag) {
    // in the menu and in a match: a resource loaded before the pick (emblem ring, screen
    // shader) stays in the cache with the old swap otherwise
    if (name && g_SetResourceName) {
        const char* cur = BufStr_Get(name);
        size_t len = strlen(cur);
        std::string repl;
        // not effects in a match: a regular match gives only what its manifest holds, and
        // an item's effect asked for by its own name came back as the error effect (red
        // crosses); its data is swapped in by InitForFileRead under the original name anyway
        bool effect = len > 5 && _stricmp(cur + len - 5, ".vpcf") == 0;
        if (len > 5 && IsSwappableResource(cur) && !(effect && !g_inMenu) && FindReplacementFile(cur, repl) && IsResident(repl)) {
            ResourceName other;
            if (g_SetResourceName(&other, repl.c_str())) {
                void* r = g_OrigFindOrRegister(rs, &other, flag);
                if (r) { InterlockedIncrement(&g_substituted); return r; }
            }
        }
    }
    return g_OrigFindOrRegister(rs, name, flag);
}

static const uint8_t kFindOrRegisterSig[] = {
    0x48, 0x89, 0x5C, 0x24, 0x18, 0x48, 0x89, 0x74, 0x24, 0x20, 0x57, 0x48, 0x81, 0xEC, 0xA0, 0x00,
    0x00, 0x00, 0xF7, 0x02, 0xFF, 0xFF, 0xFF, 0x3F, 0x41, 0x0F, 0xB6, 0xF8, 0x48, 0x8B, 0xDA, 0x48, 0x8B, 0xF1
};
static const size_t kFindOrRegisterStolen = 18;

// mov [rsp+18],rbx; push rbp/rsi/rdi/r12/r13; lea rbp,[rsp-40]; sub rsp,140;
// mov rbx,[rbp+90]; xor r13d,r13d; mov [rsp+70],r13d; movzx r12d,r9b
static const uint8_t kInitForFileReadSig[] = {
    0x48, 0x89, 0x5C, 0x24, 0x18, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55,
    0x48, 0x8D, 0x6C, 0x24, 0xC0, 0x48, 0x81, 0xEC, 0x40, 0x01, 0x00, 0x00,
    0x48, 0x8B, 0x9D, 0x90, 0x00, 0x00, 0x00, 0x45, 0x33, 0xED,
    0x44, 0x89, 0x6C, 0x24, 0x70, 0x45, 0x0F, 0xB6, 0xE1
};
// The first 17 bytes (up to sub rsp) have no RIP-relative instructions: they move into the trampoline
static const size_t kStolenBytes = 17;
// At offset +0x58: lea rcx,[rip+disp32], the address of the global ResourceSystem object
static const size_t kResourceSystemLeaOffset = 0x58;

static uint8_t* FindPattern(HMODULE mod, const uint8_t* sig, size_t sigLen) {
    MODULEINFO mi{};
    if (!GetModuleInformation(GetCurrentProcess(), mod, &mi, sizeof(mi))) return nullptr;
    uint8_t* base = (uint8_t*)mi.lpBaseOfDll;
    uint8_t* found = nullptr;
    for (size_t i = 0; i + sigLen <= mi.SizeOfImage; i++) {
        if (memcmp(base + i, sig, sigLen) == 0) {
            if (found) return nullptr;  // the signature is not unique: do not risk it
            found = base + i;
        }
    }
    return found;
}

static void WriteAbsJmp(uint8_t* at, void* dest) {
    // jmp qword ptr [rip+0]; dq dest
    at[0] = 0xFF; at[1] = 0x25;
    *(uint32_t*)(at + 2) = 0;
    *(void**)(at + 6) = dest;
}

// stolen: length of whole instructions at the function's start (>= 14, no RIP-relative)
static bool InstallHook(uint8_t* target, size_t stolen, void* hook, void** original) {
    uint8_t* tramp = (uint8_t*)VirtualAlloc(nullptr, 64, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    if (!tramp) return false;
    memcpy(tramp, target, stolen);
    WriteAbsJmp(tramp + stolen, target + stolen);

    DWORD old;
    if (!VirtualProtect(target, stolen, PAGE_EXECUTE_READWRITE, &old)) return false;
    uint8_t patch[32];
    memset(patch, 0x90, sizeof(patch));
    WriteAbsJmp(patch, hook);
    *original = tramp;
    memcpy(target, patch, stolen);
    VirtualProtect(target, stolen, old, &old);
    FlushInstructionCache(GetCurrentProcess(), target, stolen);
    return true;
}

// --- «SELECT» presses ---
// Written to equips.txt (`tick|class|from|def|labels`); the launcher puts the item on
// by the row's item index or name.
struct EquipEvent { ULONGLONG tick; uint32_t cls; ULONGLONG from; uint32_t def; std::string labels; };
static std::vector<EquipEvent> g_equips;
static SRWLOCK g_equipsLock = SRWLOCK_INIT;
static volatile LONG g_equipsDirty = 0;

static DWORD WINAPI WriteEquips(LPVOID) {
    std::string path = g_appDir + "equips.txt";
    std::string tmp = path + ".tmp";
    for (;;) {
        Sleep(30);
        if (!InterlockedExchange(&g_equipsDirty, 0)) continue;
        AcquireSRWLockShared(&g_equipsLock);
        auto all = g_equips;
        ReleaseSRWLockShared(&g_equipsLock);
        FILE* f = fopen(tmp.c_str(), "w");
        if (!f) continue;
        for (auto& e : all) fprintf(f, "%llu|%u|%llu|%u|%s\n", e.tick, e.cls, e.from, e.def, e.labels.c_str());
        fclose(f);
        MoveFileExA(tmp.c_str(), path.c_str(), MOVEFILE_REPLACE_EXISTING);
    }
}

// --- The «SELECT» button on Dota's item cards (Panorama) ---
// Hooks CUIEngine::RunScript (panorama.dll): from the interface thread, inside
// one of the game's own calls, runs our JS. It finds the open pages
// with item cards and adds the «SELECT» button. A press
// sends a DSL_PICK message (or runs the /*DSL_PICK*/ marker script), which
// this DLL writes to equips.txt: the launcher puts on the item of the row.
using RunScriptFn = void (*)(void* engine, void* panel, const char* script, const char* path, uint64_t lineCol);
static RunScriptFn g_OrigRunScript = nullptr;
static thread_local bool t_inOurScript = false;
// when the script was last offered to each panel's context: every 5 s per panel. (One
// pause for all panels let the menu's frequent scripts take every turn, and the match
// HUD, which runs scripts mostly while loading, got no loop: no hero for the taunts.)
static std::unordered_map<void*, ULONGLONG> g_lastInject;

static const char kPickJs[] = R"JS(
if (typeof DSLTxt === "undefined") DSLTxt = { select: "SELECT", selected: "SELECTED", have: "OWNED", restart: "AFTER RESTART" };
(function () {
  var ctx = $.GetContextPanel();
  if (!ctx) return;
  var root = ctx;
  while (root.GetParent()) root = root.GetParent();
  var me = "" + Math.random();
  // a live loop (heartbeat newer than 2 s) is not replaced
  var hb = parseFloat(root.GetAttributeString("dsl_hb", "0")) || 0;
  if (Date.now() - hb < 2000) return;
  root.SetAttributeString("dsl_loop", me);
  // The menu (dashboard) and the match HUD are separate panel trees, each with a loop
  // of its own: the DLL sends each loop what it needs. (Once the HUD loop took every
  // call, the menu loop never got the slot icons or the model refresh again.)
  var loopKind = (root.paneltype === "DOTADashboard" || root.FindChildTraverse("DashboardCore") ||
              root.FindChildrenWithClassTraverse("DashboardPage").length > 0 || /dashboard/i.test(root.id || "")) ? "dash" : "hud";
  var last = "";
  var lastMenu = null;
  var lastHero = "", lastHeroWhy = "";
  // Your hero's entity. In a regular match the game gives none for the player: the
  // unit in the portrait, or a real hero of yours, stands in.
  DSLLocalHero = function () {
    var me = Players.GetLocalPlayer();
    var hero = Players.GetPlayerHeroEntityIndex(me);
    if (!(hero > 0) && Players.GetLocalPlayerPortraitUnit) {
      var pu = Players.GetLocalPlayerPortraitUnit();
      if (pu > 0 && Entities.IsRealHero(pu) && Entities.GetPlayerOwnerID(pu) === me) hero = pu;
    }
    if (!(hero > 0)) {
      var hs = Entities.GetAllHeroEntities() || [];
      for (var i = 0; i < hs.length && !(hero > 0); i++)
        if (Entities.IsRealHero(hs[i]) && Entities.GetPlayerOwnerID(hs[i]) === me) hero = hs[i];
    }
    return hero;
  };
  // A hero's npc name. In a regular match the game gives no unit names (empty), so the
  // player's picked hero or the entity's class name (npc_dota_hero_x) stands in.
  DSLHeroName = function (ent, player) {
    var n = "";
    try { n = Entities.GetUnitName(ent) || ""; } catch (e) {}
    if (!n && player >= 0 && Players.GetPlayerSelectedHero) { try { n = Players.GetPlayerSelectedHero(player) || ""; } catch (e2) {} }
    if (!n) { try { n = Entities.GetClassname(ent) || ""; } catch (e3) {} }
    return n.indexOf("npc_dota_hero_") === 0 ? n : "";
  };
  function tick() {
    try {
      if (!root.IsValid() || root.GetAttributeString("dsl_loop", "") !== me) return;
      root.SetAttributeString("dsl_hb", "" + Date.now());
      var pages = root.FindChildrenWithClassTraverse("DashboardPage");
      var types = [];
      for (var i = 0; i < pages.length; i++) {
        var page = pages[i];
        types.push(page.paneltype);
        if (page.paneltype !== "DOTAStoreItemDetailsPage") continue;
        if (page.FindChildTraverse("DSLPickButton")) continue;
        var box = page.FindChildrenWithClassTraverse("StorePurchaseButtons")[0];
        if (!box) { var pb = page.FindChildTraverse("StoreItemPurchase"); if (pb) box = pb.GetParent(); }
        if (!box) box = page.FindChildrenWithClassTraverse("FooterButtonContainer")[0];
        if (!box) { types.push("(no box, children=" + page.GetChildCount() + ")"); continue; }
        types.push("(box " + box.id + "/" + box.paneltype + ")");
        var b = $.CreatePanel("Button", box, "DSLPickButton");
        b.AddClass("ButtonPrimary");
        b.AddClass("Green");
        b.AddClass("Large");
        b.style.marginLeft = "8px";
        var l = $.CreatePanel("Label", b, "");
        l.text = DSLTxt.select;
        (function (b, l) {
          b.SetPanelEvent("onactivate", function () {
            $.Msg("DSL_PICK");
            l.text = DSLTxt.selected;
            $.Schedule(3, function () { if (b.IsValid()) l.text = DSLTxt.select; });
          });
        })(b, l);
        $.Msg("DSL_LOG button added");
      }
      for (var j = 0; j < pages.length; j++) {
        var hp = pages[j];
        // any open menu page with item cards: the hero page,
        // «Environment» (wards, creeps, towers, emblems…), the armory
        if (!hp.visible || hp.paneltype === "DOTAStoreItemDetailsPage") continue;
        // item cards: each has an ItemInfoContainer (unavailable ones have no buy button)
        var stack = [hp];
        var infos = [];
        while (stack.length) {
          var p = stack.pop();
          if (p.id === "ItemInfoContainer") { infos.push(p); continue; }
          for (var c = 0; c < p.GetChildCount(); c++) stack.push(p.GetChild(c));
        }
        var added = 0;
        for (var k = 0; k < infos.length; k++) {
          var box = infos[k];
          var row = box.GetParent();
          var card = row;
          while (card && card.paneltype !== "DOTAEconItemAnimated") card = card.GetParent();
          if (!row || !card) continue;
          var unavailable = card.BHasClass("Unavailable");
          var nativeStatus = box.FindChild("StatusLabelContainer");
          var statusText = nativeStatus && nativeStatus.FindChild("StatusLabel0") ? nativeStatus.FindChild("StatusLabel0").text : "";
          // an owned item: class Owned; the status text («OWNED») counts only if the card is not «UNAVAILABLE»
          var owned = DSLOwned(box) || (!!statusText && !unavailable);
          var wrapHave = box.FindChild("DSLPickWrap");
          if (wrapHave) wrapHave.visible = !owned;
          if (nativeStatus && nativeStatus.GetAttributeString("dsl_hidden", "") === "1" && owned) {
            nativeStatus.visible = true;
            nativeStatus.SetAttributeString("dsl_hidden", "");
          }
          if (owned) continue;
          // hide the buy block with the price; on unavailable cards Dota's own «UNAVAILABLE» and the dimming
          var multi = box.FindChild("AvailableItemPurchaseButton");
          if (multi) multi.visible = false;
          if (unavailable && nativeStatus) { nativeStatus.visible = false; nativeStatus.SetAttributeString("dsl_hidden", "1"); }
          card.style.opacity = "1";
          card.style.brightness = "1";
          var chain = [row.paneltype + "#" + row.id];
          var have = box.FindChildTraverse("DSLPickButton");
          if (have) { DSLSetPicked(have, have.GetChild(0), !!picked[DSLKey(row)]); continue; }
          // as on Dota's card: a status line at the bottom; on hover over the card
          // it fades and a full-width «SELECT» button takes its place
          var wrap = $.CreatePanel("Panel", box, "DSLPickWrap");
          wrap.style.width = "100%";
          wrap.style.verticalAlign = "bottom";
          var st = $.CreatePanel("Panel", wrap, "DSLStatus");
          st.style.width = "100%";
          st.style.padding = "6px 0px";
          st.style.marginTop = "8px";
          st.style.marginBottom = "1px";
          st.style.borderTop = "1px solid #ffffff0a";
          var sl = $.CreatePanel("Label", st, "DSLStatusText");
          sl.text = DSLTxt.have;
          sl.style.horizontalAlign = "center";
          sl.style.fontSize = "18px";
          sl.style.fontWeight = "semi-bold";
          sl.style.color = "rgba(255, 255, 255, 0.5)";
          sl.style.textShadow = "0px 1px 4px rgba(0, 0, 0, 0.2)";
          var sd = $.CreatePanel("Label", st, "DSLStatusDone");
          sd.text = DSLTxt.selected;
          sd.style.horizontalAlign = "center";
          sd.style.fontSize = "18px";
          sd.style.fontWeight = "semi-bold";
          sd.style.color = "rgba(255, 255, 255, 0.9)";
          sd.style.textShadow = "0px 0px 12px rgba(224, 249, 255, 0.4)";
          sd.visible = false;
          var b = $.CreatePanel("Button", wrap, "DSLPickButton");
          b.AddClass("ButtonPrimary");
          b.AddClass("DarkGrey");
          b.style.width = "100%";
          b.style.verticalAlign = "bottom";
          b.visible = false;
          var l = $.CreatePanel("Label", b, "DSLPlain");
          l.style.horizontalAlign = "center";
          l.text = DSLTxt.select;
          hoverItems.push({ b: b, st: st, row: row });
          DSLSetPicked(b, l, !!picked[DSLKey(row)]);
          (function (b, l, row) {
            b.SetPanelEvent("onactivate", function () {
              if (!row || !row.IsValid()) return;
              var rowLabels = DSLLabels(row);
              var key = rowLabels.join("|");
              if (picked[key]) return;
              var def = DSLItemDef(row);
              // the list's other rows go back to «SELECT» (one item per slot)
              // rows are nested in wrappers: go up to the container of the whole list
              var list = row.GetParent(), others = [];
              for (var up = 0; list && up < 8; up++, list = list.GetParent()) {
                others = DSLButtons(list);
                if (others.length > 1) break;
              }
              for (var c = 0; c < others.length; c++) {
                var ob = others[c];
                if (ob === b) continue;
                delete picked[DSLKey(DSLRowOf(ob))];
                DSLSetPicked(ob, ob.GetChild(0), false);
              }
              picked[key] = true;
              DSLSetPicked(b, l, true);
              $.DispatchEvent("Activated", row, "mouse");
              $.Msg("DSL_PICK " + def + "|" + rowLabels.join("\t"));
            });
          })(b, l, row);
          added++;
          if (k === 0) types.push("chain=" + chain.join(">") + " row=" + (row ? row.paneltype + "#" + row.id : "-"));
        }
        if (added) types.push(hp.paneltype + " buttons +" + added);
      }
      var inMenu = false;
      try { inMenu = typeof Game !== "undefined" && Game.GetState() === 0; } catch (e2) {}
      if (inMenu !== lastMenu) { lastMenu = inMenu; $.Msg("DSL_MENU " + (inMenu ? 1 : 0)); }
      // your hero in a match (for taunts): entity index and unit name
      if (!inMenu) {
        var heroWhy = "";
        try {
          var me2 = Players.GetLocalPlayer();
          var hero = Players.GetPlayerHeroEntityIndex(me2);
          heroWhy = "player " + me2 + ", hero " + hero;
          // not given (a regular match): the unit in the portrait, or a real hero of yours
          if (!(hero > 0) && Players.GetLocalPlayerPortraitUnit) {
            var pu = Players.GetLocalPlayerPortraitUnit();
            heroWhy += ", portrait " + pu;
            if (pu > 0 && Entities.IsRealHero(pu) && Entities.GetPlayerOwnerID(pu) === me2) hero = pu;
          }
          if (!(hero > 0)) {
            var hs = Entities.GetAllHeroEntities() || [];
            heroWhy += ", heroes " + hs.length;
            for (var hi = 0; hi < hs.length && !(hero > 0); hi++)
              if (Entities.IsRealHero(hs[hi]) && Entities.GetPlayerOwnerID(hs[hi]) === me2) hero = hs[hi];
          }
          var heroKey = hero > 0 ? hero + "|" + DSLHeroName(hero, me2) : "";
          if (heroKey && heroKey !== lastHero) { lastHero = heroKey; $.Msg("DSL_HERO " + heroKey); }
        } catch (e3) { heroWhy += " error " + e3; }
        if (!lastHero && heroWhy !== lastHeroWhy) { lastHeroWhy = heroWhy; $.Msg("DSL_LOG no hero yet (" + loopKind + "): " + heroWhy); }
      }
      // the persona switch (CLASSIC / persona): for a persona you do not own Dota's button
      // is disabled: enable it and tell the launcher «toggle the hero's persona»
      if (inMenu) {
        try {
          var hpp = root.FindChildrenWithClassTraverse("DashboardPage");
          for (var pi2 = 0; pi2 < hpp.length; pi2++) {
            var hpg = hpp[pi2];
            if (hpg.paneltype !== "DOTAHeroPageNewV2" || !hpg.visible) continue;
            var sels = ["PersonaSelector", "PersonaSelector2"];
            for (var si = 0; si < sels.length; si++) {
              var sel = hpg.FindChildTraverse(sels[si]);
              var pb = sel ? sel.FindChildTraverse("PersonaInvisibleButton") : null;
              if (!pb) continue;
              if (!pb.enabled) pb.enabled = true;
              if (pb.GetAttributeString("dsl_persona", "") === "1") continue;
              pb.SetAttributeString("dsl_persona", "1");
              (function (page) {
                pb.SetPanelEvent("onactivate", function () {
                  var hn = page.FindChildTraverse("HeroName");
                  var lab = hn ? hn.FindChildrenWithClassTraverse("TitleExtraLarge")[0] : null;
                  var name = lab ? lab.text : "";
                  $.Msg("DSL_LOG persona toggle for " + name);
                  $.Msg("DSL_PICK 0|@persona	" + name);
                });
              })(hpg);
            }
          }
        } catch (e12) { $.Msg("DSL_LOG persona error " + e12); }
      }
      // item glows: on your own hero in a match, on the hero models in the menu
      $.Msg("DSL_GTICK " + loopKind);
      // glows from one loop: the menu's in the menu, the HUD's in a match (the other one
      // forgets its effects: their models go away with the map change)
      if ((loopKind === "dash") === inMenu) DSLGlowTick(inMenu); else DSLForgetGlows();
      if (inMenu) DSLSlotTick();
      var log = types.filter(function (t) { return t.indexOf("DOTA") !== 0; }).join(", ");
      if (log && log !== last) { last = log; $.Msg("DSL_LOG " + log); }
    } catch (e) {
      $.Msg("DSL_LOG error " + e);
    }
    $.Schedule(0.5, tick);
  }
  // the row's item definition index: a property of the DOTAEconItem element
  DSLItemDef = function (row) {
    var stack = [row], seen = [];
    while (stack.length) {
      var p = stack.pop();
      var vals = [p.itemdef, p.itemDef, p.ItemDef, p.itemdefindex, p.GetAttributeInt ? p.GetAttributeInt("itemdef", 0) : 0];
      for (var v = 0; v < vals.length; v++) {
        var n = parseInt(vals[v], 10);
        if (n > 0) { $.Msg("DSL_LOG item def " + n + " from " + p.paneltype + "#" + p.id); return n; }
      }
      if (seen.length < 25) seen.push(p.paneltype + "#" + p.id);
      for (var c = 0; c < p.GetChildCount(); c++) stack.push(p.GetChild(c));
    }
    $.Msg("DSL_LOG no item def in row: " + seen.join(" "));
    return 0;
  };
  // the row's texts: the item's or set's name among them
  // the row's texts (without our button): the item's or set's name among them
  DSLLabels = function (row) {
    var out = [], stack = [row];
    while (stack.length) {
      var p = stack.pop();
      if (p.id === "DSLPickButton") continue;
      if (p.paneltype === "Label" && p.text) out.push(p.text.replace(/[|\t\r\n]/g, " "));
      for (var c = p.GetChildCount() - 1; c >= 0; c--) stack.push(p.GetChild(c));
    }
    return out;
  };
  DSLKey = function (row) { return row && row.IsValid() ? DSLLabels(row).join("|") : ""; };
  // all our buttons inside the panel
  DSLButtons = function (root) {
    var out = [], stack = [root];
    while (stack.length) {
      var p = stack.pop();
      if (p.id === "DSLPickButton") { out.push(p); continue; }
      for (var c = 0; c < p.GetChildCount(); c++) stack.push(p.GetChild(c));
    }
    return out;
  };
  // the button's item row: the ancestor whose parent holds the ItemInfoContainer
  DSLRowOf = function (btn) {
    var row = btn.GetParent();
    for (var u = 0; u < 6 && row; u++) {
      if (row.GetParent() && row.GetParent().FindChild("ItemInfoContainer") === row) return row.GetParent();
      row = row.GetParent();
    }
    return null;
  };
  var picked = {};  // row key -> picked
  // «SELECT» is a dark grey button, as Dota's; «SELECTED» is a label without a background
  // as on Dota's card (ui_econ_item_animated): «SELECT» is a ButtonPrimary DarkGrey button
  // across the card's bottom; «SELECTED» is the status line (white text with a glow)
  // «SELECTED» is in the status line (white text with a glow), no button on hover
  DSLSetPicked = function (b, l, on) {
    if (!b) return;
    var state = on ? "on" : "off";
    if (b.GetAttributeString("dsl_state", "") === state) return;
    b.SetAttributeString("dsl_state", state);
    b.SetAttributeInt("dsl_rver", -1);
    var st = b.GetParent() ? b.GetParent().FindChild("DSLStatus") : null;
    if (!st) return;
    st.FindChild("DSLStatusText").visible = !on;
    st.FindChild("DSLStatusDone").visible = on;
    if (on) { b.visible = false; st.style.opacity = "1"; }
  };
  // items for empty slots that show after a Dota restart: a picked card says
  // «AFTER RESTART» instead of «SELECTED». The DLL calls this from inside its own
  // call: only remember here, the tick changes the labels
  var restartNames = {}, restartVer = 0, pendingRestart = null;
  DSLSetRestart = function (list) { pendingRestart = list || []; };
  function doneText(it) {
    var labels = DSLLabels(it.row);
    for (var i = 0; i < labels.length; i++) {
      if (restartNames[("" + labels[i]).trim().toLowerCase()]) return true;
    }
    return false;
  }
  // --- the preset's items in the hero page's slot tiles ---
  // A tile (DOTAHeroSlotItemPicker[Loadout]: Header > SlotName, SlotEconItem) is known
  // by its slot caption; the hero by the page's hero name. The DLL gives
  // {HERO: {CAPTION: [item, style, icon, rarity]}}; the tile shows that item.
  var slotMap = {}, pendingSlots = null, slotForceNext = false, slotHero = "", slotErr = false;
  // Leaving or entering the wardrobe makes the game fill the tiles with the real
  // loadout again, in the same panels (nothing to notice on them): a change of the
  // page's view, seen on the fast tick, puts the icons back once, 0.1 s later.
  var slotViewPage = null, slotViewPanels = [], slotView = "", slotViewPending = false;
  function slotViewKey() {
    return slotViewPanels.map(function (pn) {
      return pn && pn.IsValid() ? (pn.visible ? 1 : 0) + "" + (pn.actuallayoutheight > 0 ? 1 : 0) : "-";
    }).join(",");
  }
  function slotViewTick() {
    if (!slotViewPage || !slotViewPage.IsValid() || slotViewPending) return;
    var key = slotViewKey();
    if (key === slotView) return;
    slotView = key;
    slotViewPending = true;
    slotWatch(800);
    $.Schedule(0.05, function () { slotViewPending = false; slotRepair(); DSLSlotTick(); });
  }
  // right after we asked the game to redraw the tiles (our model refresh): it does
  // so a frame or two later, so the tiles are watched on every frame for a moment
  DSLSlotRedrawn = function () { slotWatch(800); };
  DSLSetSlots = function (m) {
    pendingSlots = m || {};
    $.Schedule(0, function () { DSLSlotTick(); });
  };
  function slotKey(s) { return ("" + s).toUpperCase().replace(/[^A-Z0-9\u0400-\u04FF]/g, ""); }
  function firstLabel(p) {
    var stack = [p];
    while (stack.length) {
      var q = stack.pop();
      if (q.paneltype === "Label" && q.text) return q.text;
      for (var c = q.GetChildCount() - 1; c >= 0; c--) stack.push(q.GetChild(c));
    }
    return "";
  }
  // Our item in a tile. Dota's econ item is set to it once (its tooltip, its style);
  // our icon lies on Dota's image as a panel of our own, the default item's grey wash
  // is cancelled and the rarity strip coloured, as inline styles. Dota redrawing the
  // tile with the real loadout (after every model refresh, a frame or two later)
  // then shows nothing: that redraw showing through, and the reload of the image when
  // the item was set again, were the flicker.
  var kRarityColors = { common: "#b0c3d9", uncommon: "#5e98d9", rare: "#4b69ff", mythical: "#8847ff",
    legendary: "#d32ce6", ancient: "#eb4b4b", immortal: "#e4ae39", arcana: "#ade55c", seasonal: "#fff34f" };
  function slotDress(econ, m) {
    if (econ.GetAttributeString("dsl_dressed", "") !== "1") {
      econ.SetAttributeString("dsl_dressed", "1");
      // what the game's .IsDefaultItem changes (saturation 0, a blue wash)
      econ.style.saturation = "1";
      econ.style.washColor = "none";
    }
    var color = kRarityColors[m[3]];
    var bottom = econ.FindChild("BottomLayer");
    if (bottom && color && bottom.GetAttributeString("dsl_color", "") !== color) {
      bottom.style.backgroundColor = color;
      bottom.SetAttributeString("dsl_color", color);
    }
    var icon = m[2];
    if (!icon) return;  // an item in a style: its own icon, Dota's image is left
    var img = econ.FindChild("EconItemIcon");
    var ov = econ.FindChild("DSLSlotIcon");
    if (!ov) {
      // as Dota's own image (#EconItemIcon in ui_econ_item.css)
      ov = $.CreatePanel("Image", econ, "DSLSlotIcon");
      ov.hittest = false;
      ov.SetScaling("stretch-to-cover-preserve-aspect");
      ov.style.width = "100%";
      ov.style.height = "100%";
      ov.style.horizontalAlign = "center";
      ov.style.marginBottom = "2px";
      ov.style.saturation = "0.7";
    }
    if (ov.GetAttributeString("dsl_icon", "") !== icon) {
      ov.SetImage(icon);
      ov.SetAttributeString("dsl_icon", icon);
    }
    if (!ov.visible) ov.visible = true;
    // right over Dota's image: under the border, the style switch and the badges
    // (kept there: an image Dota rebuilds may come after ours)
    if (img) {
      var indexed = typeof econ.GetChildIndex === "function";
      if (indexed ? econ.GetChildIndex(ov) !== econ.GetChildIndex(img) + 1 : ov.GetAttributeString("dsl_moved", "") !== "1") {
        econ.MoveChildAfter(ov, img);
        ov.SetAttributeString("dsl_moved", "1");
      }
    }
  }
  // the tile shows the game's own item again (our image is hidden, not deleted: a
  // deletion lands a frame later, after the next hero's icon may have reused it)
  function slotUndress(econ) {
    if (!econ || !econ.IsValid()) return;
    var ov = econ.FindChild("DSLSlotIcon");
    if (ov) ov.visible = false;
    if (econ.GetAttributeString("dsl_dressed", "") === "1") {
      econ.style.saturation = null;
      econ.style.washColor = null;
      econ.SetAttributeString("dsl_dressed", "");
    }
    var bottom = econ.FindChild("BottomLayer");
    if (bottom && bottom.GetAttributeString("dsl_color", "")) {
      bottom.style.backgroundColor = null;
      bottom.SetAttributeString("dsl_color", "");
    }
    econ.SetAttributeInt("dsl_def", 0);
  }
  // Dota's econ item only when the item changes (setting it reloads the image)
  function slotSet(econ, m) {
    if (econ.GetAttributeInt("dsl_def", 0) !== m[0]) {
      if (m[1] > 0 && econ.SetItemByDefinitionAndStyle) econ.SetItemByDefinitionAndStyle(m[0], m[1]);
      else econ.SetItemByDefinition(m[0]);
      econ.SetAttributeInt("dsl_def", m[0]);
    }
    slotDress(econ, m);
  }
  // the tiles of the visible page with our item: kept dressed on every fast tick
  // (50 ms), and on every frame for a moment after the game redraws them (our model
  // refresh, a pick, the wardrobe opening): a redraw that rebuilt the tile's panels
  // is covered again within a frame
  var slotTiles = [], slotTilesHero = "", slotHeroLabel = null;
  var slotWatchUntil = 0, slotWatching = false;
  function slotWatch(ms) {
    slotWatchUntil = Math.max(slotWatchUntil, Date.now() + ms);
    if (slotWatching) return;
    slotWatching = true;
    (function frame() {
      slotRepair();
      if (Date.now() < slotWatchUntil) $.Schedule(0, frame);
      else slotWatching = false;
    })();
  }
  // Another hero on the page: the game reuses the same tiles for that hero's
  // items, so the old hero's tiles are dropped at once (the repair must not put
  // them back), and the game is asked to redraw the page if we had changed it.
  function slotHeroTick() {
    if (!slotHeroLabel || !slotHeroLabel.IsValid()) return;
    var name = slotHeroLabel.text;
    if (name === slotTilesHero) return;
    var had = slotTiles.length > 0;
    // the same panels show the new hero's items: ours come off them
    for (var i = 0; i < slotTiles.length; i++) slotUndress(slotTiles[i].econ);
    slotTiles = [];
    slotTilesHero = name;
    if (had) $.DispatchEvent("DOTALoadoutChanged");
    $.Schedule(0.05, function () { DSLSlotTick(); });
  }
  function slotRepair() {
    for (var i = 0; i < slotTiles.length; i++) {
      var s = slotTiles[i];
      if (!s.econ.IsValid()) continue;
      try { slotDress(s.econ, s.m); }
      catch (e) { if (!slotErr) { slotErr = true; $.Msg("DSL_LOG slot icon error " + e); } }
    }
  }
  DSLSlotTick = function () {
    // Dota's econ item is set only when the tile's item changes (a new tile, a new
    // pick, another hero); our icon and colours are kept on the tile all the time
    var force = slotForceNext;
    slotForceNext = false;
    if (pendingSlots) { slotMap = pendingSlots; pendingSlots = null; force = true; }
    var pages = root.FindChildrenWithClassTraverse("DashboardPage");
    var shown = [];
    for (var i = 0; i < pages.length; i++) {
      var hp = pages[i];
      if (hp.paneltype !== "DOTAHeroPageNewV2" || !hp.visible) continue;
      var nameP = hp.FindChildTraverse("HeroName");
      var heroName = nameP ? firstLabel(nameP) : "";
      if (nameP && (!slotHeroLabel || !slotHeroLabel.IsValid() || slotHeroLabel.text !== heroName)) {
        var st = [nameP];
        slotHeroLabel = null;
        while (st.length && !slotHeroLabel) {
          var lp = st.pop();
          if (lp.paneltype === "Label" && lp.text === heroName) slotHeroLabel = lp;
          for (var lc = lp.GetChildCount() - 1; lc >= 0; lc--) st.push(lp.GetChild(lc));
        }
      }
      var hero = slotMap[slotKey(heroName)];
      if (heroName !== slotHero) { slotHero = heroName; force = true; }
      // the panels whose look tells whether the wardrobe is open (watched by slotViewTick)
      if (slotViewPage !== hp) {
        slotViewPage = hp;
        slotViewPanels = ["HeroCustomize", "CosmeticPicker", "EquippedSetDetailsContainer", "EquippedSetSlotsContainer"]
          .map(function (id) { return hp.FindChildTraverse(id); });
        slotView = slotViewKey();
      }
      // slot captions (wardrobe list) and every "equipped" row on the page
      var labels = [], rows = [], stack = [hp];
      while (stack.length) {
        var q = stack.pop();
        if (q.id === "SlotName" && q.paneltype === "Label") { labels.push(q); continue; }
        if (q.id === "EquippedSetSlotsContainer") rows.push(q);
        for (var c = 0; c < q.GetChildCount(); c++) stack.push(q.GetChild(c));
      }
      // a tile with no item of ours (left the preset) shows the game's own again
      for (var k = 0; k < labels.length; k++) {
        var tile = labels[k].GetParent() ? labels[k].GetParent().GetParent() : null;
        var econ = tile ? tile.FindChildTraverse("SlotEconItem") : null;
        if (!econ) continue;
        var m = hero ? hero[slotKey(labels[k].text)] : null;
        if (!m) {
          if (econ.GetAttributeInt("dsl_def", 0)) slotUndress(econ);
          continue;
        }
        shown.push({ econ: econ, m: m });
        try { slotSet(econ, m); }
        catch (e) { if (!slotErr) { slotErr = true; $.Msg("DSL_LOG slot icon error " + e); } }
      }
      // the "equipped" row: DOTAEconItem tiles without captions, in the hero
      // script's slot order (only when the count matches, e.g. not with a persona)
      for (var w = 0; w < rows.length; w++) {
        var tiles = [];
        for (var c2 = 0; c2 < rows[w].GetChildCount(); c2++)
          if (rows[w].GetChild(c2).paneltype === "DOTAEconItem") tiles.push(rows[w].GetChild(c2));
        var fits = !!(hero && hero["#LEN"] && tiles.length === hero["#LEN"][0]);
        for (var r = 0; r < tiles.length; r++) {
          var mr = fits ? hero["#" + r] : null;
          if (!mr) {
            if (tiles[r].GetAttributeInt("dsl_def", 0)) slotUndress(tiles[r]);
            continue;
          }
          shown.push({ econ: tiles[r], m: mr });
          try { slotSet(tiles[r], mr); }
          catch (e5) { if (!slotErr) { slotErr = true; $.Msg("DSL_LOG slot icon error " + e5); } }
        }
      }
    }
    slotTiles = shown;
    slotTilesHero = slotHeroLabel && slotHeroLabel.IsValid() ? slotHeroLabel.text : "";
    // Dota fills a new or re-picked tile a frame or two later: covered right away
    if (force && shown.length) slotWatch(800);
  };
  // hover over a card (like Dota's .Container:hover): the button instead of the status line
  var hoverItems = [];
  function hoverTick() {
    var alive = [];
    for (var i = 0; i < hoverItems.length; i++) {
      var it = hoverItems[i];
      if (!it.b.IsValid() || !it.row || !it.row.IsValid()) continue;
      alive.push(it);
      var show = it.b.GetAttributeString("dsl_state", "") !== "on" && it.row.BHasHoverStyle();
      if (it.b.visible !== show) {
        it.b.visible = show;
        it.st.style.opacity = show ? "0" : "1";
      }
    }
    hoverItems = alive;
    if (pendingRestart) {
      var names = {};
      for (var r = 0; r < pendingRestart.length; r++) names[("" + pendingRestart[r]).trim().toLowerCase()] = true;
      restartNames = names;
      pendingRestart = null;
      restartVer++;
    }
    for (var j = 0; j < alive.length; j++) {
      var a = alive[j];
      if (a.b.GetAttributeString("dsl_state", "") !== "on" || a.b.GetAttributeInt("dsl_rver", -1) === restartVer) continue;
      a.b.SetAttributeInt("dsl_rver", restartVer);
      var sd = a.st.FindChild("DSLStatusDone");
      if (!sd) continue;
      var later = doneText(a);
      sd.text = later ? DSLTxt.restart : DSLTxt.selected;
      sd.style.fontSize = later ? "14px" : "18px";
    }
    // a frequent tick for the DLL (in the menu): preloading and model refresh without waiting
    if (lastMenu !== null && root.IsValid() && root.GetAttributeString("dsl_loop", "") === me) $.Msg("DSL_TICK " + loopKind);
    if (lastMenu) { slotHeroTick(); slotViewTick(); slotRepair(); }
    $.Schedule(0.05, hoverTick);
  }
  hoverTick();
  // the Owned class is set on the card (DOTAEconItemAnimated or its Container): look among the ancestors
  DSLOwned = function (p) {
    for (var u = 0; p && u < 8; u++, p = p.GetParent()) {
      if (p.BHasClass("Owned")) return true;
      if (p.paneltype === "DOTAEconItemAnimated") break;
    }
    return false;
  };
  // --- Item glows in a match ---
  // Effects an item creates by itself (particle_create), which no file swap can place:
  // create them on your own hero with the attachments from the effect's file. The DLL gives the list.
  var glowList = [], glowFx = {};  // key "entity|effect" -> {p: effect index, ent, handle}
  // The item models the DLL found for the glows (the wearable the game hangs an item's
  // effects on): item model -> [[entity, handle, parent entity, owner entity], ...]
  var itemHosts = {};
  DSLSetItemHosts = function (m) { itemHosts = m || {}; };
  function hostListed(handle) {
    for (var m in itemHosts) {
      var l = itemHosts[m];
      for (var i = 0; i < l.length; i++) if (l[i][1] === handle) return true;
    }
    return false;
  }
  // The target hero's own item model for a glow, or null.
  function hostOf(gl, ent) {
    var l = gl.item ? itemHosts[gl.item] : null;
    for (var i = 0; l && i < l.length; i++) {
      if ((l[i][2] === ent || l[i][3] === ent) && Entities.IsValidEntity(l[i][0])) return l[i];
    }
    return null;
  }
  // An effect is removed through the API only while its entity is alive: when a map loads
  // («Try out»), the menu models are deleted, and a call on a dead entity crashes the game.
  // The game removes a deleted entity's effects itself: they are just forgotten. (An item
  // model lives while the DLL lists its handle: its index may belong to a new entity.)
  function dropGlow(k) {
    var g = glowFx[k];
    if (g && Entities.IsValidEntity(g.ent) && (g.handle < 0 || hostListed(g.handle))) {
      try { Particles.DestroyParticleEffect(g.p, false); Particles.ReleaseParticleIndex(g.p); } catch (e6) {}
    }
    delete glowFx[k];
  }
  DSLClearGlows = function () {
    for (var k in glowFx) dropGlow(k);
    glowFx = {};
  };
  // the other loop shows the glows now: ours are left to the game (it removes them with
  // their models; a call on a removed model's effect crashes it)
  DSLForgetGlows = function () { glowFx = {}; };
  var glowHostLogged = {};
  // The DLL calls this from inside another JS call: only remember the list here,
  // the regular tick changes the effects (otherwise the game crashes if a model is being rebuilt)
  var pendingGlows = null;
  DSLSetGlows = function (list) {
    pendingGlows = list || [];
  };
  DSLGlowTick = function (inMenu) {
    if (typeof Particles === "undefined") return;
    if (pendingGlows) {
      DSLClearGlows();
      glowList = pendingGlows;
      pendingGlows = null;
      glowHostLogged = {};
      $.Msg("DSL_LOG glows: " + glowList.length);
      return;  // new effects on the next tick
    }
    var alive = {};
    if (glowList.length) {
      var wanted = {};
      for (var g0 = 0; g0 < glowList.length; g0++) wanted[glowList[g0].npc] = true;
      var targets = [];
      if (inMenu) {
        // hero models on the menu pages: client entities with the hero's name
        var ents = Entities.GetAllEntities();
        for (var i = 0; i < ents.length; i++) {
          if (!Entities.IsValidEntity(ents[i])) continue;
          var nm = "";
          try { nm = Entities.GetUnitName(ents[i]); } catch (e8) {}
          if (nm && wanted[nm]) targets.push([ents[i], nm]);
        }
      } else {
        // in a match only your own hero: on others an effect could give away an invisible one
        var hero = DSLLocalHero();
        if (hero > 0 && Entities.IsValidEntity(hero)) targets.push([hero, DSLHeroName(hero, Players.GetLocalPlayer())]);
      }
      for (var t = 0; t < targets.length; t++) {
        var ent = targets[t][0], name = targets[t][1];
        for (var g = 0; g < glowList.length; g++) {
          var gl = glowList[g];
          if (gl.npc !== name) continue;
          // on the item's own model when the DLL found it: the whole effect, as the game
          // creates it (a snapshot skinned to the item's bones, like Twilight Schism's blade
          // outline, needs that skeleton); else on the hero, without such parts (on the
          // hero's skeleton they landed on its body)
          var host = hostOf(gl, ent);
          var on = host ? host[0] : ent;
          var fxs = host ? [gl.fx] : (gl.parts || [gl.fx]);
          for (var f = 0; f < fxs.length; f++) {
            var key = (host ? "h" + host[1] : ent) + "|" + fxs[f];
            alive[key] = true;
            if (key in glowFx) continue;
            var p = Particles.CreateParticle(fxs[f], ParticleAttachment_t.PATTACH_CUSTOMORIGIN, on);
            for (var c = 0; c < gl.cps.length; c++) {
              var cp = gl.cps[c];
              // a world-origin point is a value (its offset), not a place on the model
              if (cp[1] === "PATTACH_WORLDORIGIN") {
                var v = ("" + cp[2]).split(",").map(parseFloat);
                if (v.length !== 3 || isNaN(v[0]) || isNaN(v[1]) || isNaN(v[2])) v = [0, 0, 0];
                Particles.SetParticleControl(p, cp[0], v);
                continue;
              }
              var kind = ParticleAttachment_t[cp[1]];
              if (kind === undefined) kind = ParticleAttachment_t.PATTACH_ABSORIGIN_FOLLOW;
              Particles.SetParticleControlEnt(p, cp[0], on, kind, cp[2], [0, 0, 0], true);
            }
            glowFx[key] = { p: p, ent: on, handle: host ? host[1] : -1 };
            // once per glow and place (the wardrobe's cards re-create their models often)
            var where = (host ? "item model" : "hero") + "|" + fxs[f];
            if (!glowHostLogged[where]) { glowHostLogged[where] = true; $.Msg("DSL_LOG glow " + fxs[f] + " on the " + (host ? "item model" : "hero")); }
          }
        }
      }
    }
    for (var k in glowFx) {
      if (!alive[k]) dropGlow(k);
    }
  };
  $.Msg("DSL_LOOP " + loopKind);
  $.Msg("DSL_LOG loop started in " + ctx.id + " (" + loopKind + ", root " + root.id + ")");
  // called by the DLL when the replacements are loaded: rebuild the hero model
  DSLRefresh = function () {
    var pages = root.FindChildrenWithClassTraverse("DashboardPage");
    for (var i = 0; i < pages.length; i++) {
      var hp = pages[i];
      if (hp.paneltype !== "DOTAHeroPageNewV2" || !hp.visible) continue;
      // the same event Dota sends after «Equip»: the model is rebuilt without reloading the page
      // a try-on keeps its model over the loadout: revert it first («REVERT»,
      // DOTASetPreviewRevert). Also when the button is hidden: after leaving the
      // wardrobe the try-on stays while the panel with the button is not shown
      var revert = hp.FindChildTraverse("RevertButton");
      var controls = hp.FindChildTraverse("SetPreviewControls");
      var shown = !!(revert && revert.visible);
      var tryOn = !!revert;
      if (!DSLRefresh.logged) {
        DSLRefresh.logged = true;
        var model = hp.FindChildTraverse("FocusedHeroSetPreview");
        var api = [];
        if (model) ["ReloadScene", "ClearScene", "SetUnit", "ReplaceEconItemSlot", "SetScenePanelToLocalHero", "FireEntityInput"].forEach(function (m) { if (typeof model[m] === "function") api.push(m); });
        $.Msg("DSL_LOG refresh info: model=" + (model ? model.paneltype : "-") + " api=[" + api.join(",") + "] controls=" + (controls ? controls.visible : "-"));
      }
      if (tryOn) $.DispatchEvent("Activated", revert, "mouse");
      var redrawn = function () { if (typeof DSLSlotRedrawn === "function") DSLSlotRedrawn(); };
      // «REVERT» redraws the open slot's tiles too
      if (tryOn) redrawn();
      $.Schedule(tryOn ? 0.15 : 0, function () { $.DispatchEvent("DOTALoadoutChanged"); redrawn(); });
      // «REVERT» rebuilds the model asynchronously and may override the first refresh: repeat;
      // after a set try-on Dota brings back the old model from its cache later: once more after 2 s
      $.Schedule(0.8, function () { $.DispatchEvent("DOTALoadoutChanged"); redrawn(); });
      if (tryOn) $.Schedule(2.0, function () { $.DispatchEvent("DOTALoadoutChanged"); redrawn(); });
      $.Msg("DSL_LOG refresh: loadout changed (revert " + (revert ? (shown ? "shown" : "hidden") : "none") + ", controls " + (controls ? (controls.visible ? "shown" : "hidden") : "none") + ")");
      return;
    }
    $.Msg("DSL_LOG refresh: no hero page open (" + loopKind + " loop)");
  };
  tick();
})();
)JS";

// payload: "<def>|<label>\t<label>...": the item index and the row's texts
static void RecordPick(const char* payload) {
    uint32_t def = payload ? (uint32_t)strtoul(payload, nullptr, 10) : 0;
    std::string labels;
    if (const char* bar = payload ? strchr(payload, '|') : nullptr) {
        for (const char* c = bar + 1; *c; c++)
            labels += (*c == '|' || *c == '\n' || *c == '\r') ? ' ' : *c;
    }
    Log("[*] Panorama pick pressed (item def %u, labels: %s)", def, labels.c_str());
    AcquireSRWLockExclusive(&g_equipsLock);
    g_equips.push_back({GetTickCount64(), 0, 0, def, labels});
    if (g_equips.size() > 16) g_equips.erase(g_equips.begin());
    ReleaseSRWLockExclusive(&g_equipsLock);
    InterlockedExchange(&g_equipsDirty, 1);
}

static void* g_uiEngine = nullptr;
static void* g_injectPanel = nullptr;  // panel of the current script injection
// The script's live loops: the menu's (dashboard) and the match HUD's. They are separate
// panel trees: menu pages (slot tiles, the hero model) are reached from the menu's only.
enum LoopKind { kDash = 0, kHud = 1 };
static const char* const kLoopNames[] = {"dash", "hud"};
static void* g_loopPanels[2] = {nullptr, nullptr};

// Interface thread, from a loop's tick: a new glow table -> DSLSetGlows (each loop gets it)
static LONG g_glowsSent[2] = {-1, -1};
static void MaybeSendGlows(int kind) {
    LONG v = g_glowsVersion;
    void* panel = g_loopPanels[kind];
    if (v == g_glowsSent[kind] || !g_uiEngine || !panel || !g_OrigRunScript) return;
    g_glowsSent[kind] = v;
    AcquireSRWLockShared(&g_glowsLock);
    std::string script = "if (typeof DSLSetGlows === 'function') DSLSetGlows(" + g_glowsJs + ");"
                         "if (typeof DSLSetRestart === 'function') DSLSetRestart(" + g_restartJs + ");"
                         "if (typeof DSLSetSlots === 'function') DSLSetSlots(" + g_slotsJs + ");";
    ReleaseSRWLockShared(&g_glowsLock);
    Log("[*] Sending item glows to the game UI (%s)", kLoopNames[kind]);
    t_inOurScript = true;
    // a new name every time (and per loop): Panorama keeps a compiled script by its name,
    // and the same name ran the first table's script again (the slot icons stayed old)
    std::string name = std::string("dota_skins_table_") + kLoopNames[kind] + "_" + std::to_string(v) + ".js";
    g_OrigRunScript(g_uiEngine, panel, script.c_str(), name.c_str(), 0);
    t_inOurScript = false;
}

static void MaybeRefreshPreview() {
    static LONG lastSubs = 0;
    static ULONGLONG lastLog = 0;
    LONG subs = g_substituted;
    if (subs != lastSubs && GetTickCount64() - lastLog >= 1000) {
        Log("[*] Substituted %ld model lookups", subs - lastSubs);
        lastSubs = subs;
        lastLog = GetTickCount64();
    }
    // 1: the config changed; 2: the model is rebuilt already, waiting for the effects
    // (the hero page is the menu loop's: called from its ticks only)
    static bool modelsRefreshed = false;
    void* panel = g_loopPanels[kDash];
    if (!g_refreshPending || !g_uiEngine || !panel || !g_OrigRunScript) return;
    if (InterlockedCompareExchange(&g_refreshPending, 1, 1) == 1 && modelsRefreshed) modelsRefreshed = false;
    bool all = PreloadsDone();
    if (!all && (modelsRefreshed || !ModelsDone())) return;
    if (all) {
        InterlockedExchange(&g_refreshPending, 0);
        Log("[*] Replacements loaded, refreshing preview");
    } else {
        InterlockedExchange(&g_refreshPending, 2);
        modelsRefreshed = true;
        Log("[*] Models loaded, refreshing preview (effects still loading)");
    }
    static int refreshes = 0;
    std::string name = "dota_skins_refresh_" + std::to_string(++refreshes) + ".js";
    t_inOurScript = true;
    g_OrigRunScript(g_uiEngine, panel, "if (typeof DSLRefresh === 'function') DSLRefresh();", name.c_str(), 0);
    t_inOurScript = false;
}

static void HookedRunScript(void* engine, void* panel, const char* script, const char* path, uint64_t lineCol) {
    if (script && strstr(script, "/*DSL_LOG*/")) {
        Log("[js] %.400s", script + 11);
        return;
    }
    if (script && strstr(script, "/*DSL_PICK*/")) {
        RecordPick(nullptr);
        return;
    }
    g_OrigRunScript(engine, panel, script, path, lineCol);
    if (t_inOurScript || !panel) return;
    ULONGLONG now = GetTickCount64();
    auto last = g_lastInject.find(panel);
    if (last != g_lastInject.end() && now - last->second < 5000) return;
    if (g_lastInject.size() > 4096) {
        // panels come and go: forget the ones not seen for a while
        for (auto it = g_lastInject.begin(); it != g_lastInject.end();)
            it = now - it->second > 60000 ? g_lastInject.erase(it) : std::next(it);
    }
    g_lastInject[panel] = now;
    t_inOurScript = true;
    g_uiEngine = engine;
    g_injectPanel = panel;
    // button texts in the chosen language, before the main script
    std::string lang;
    { AcquireSRWLockShared(&g_rulesLock); lang = g_lang; ReleaseSRWLockShared(&g_rulesLock); }
    const char* texts = (lang == "en")
        ? "DSLTxt = { select: \"SELECT\", selected: \"SELECTED\", have: \"OWNED\", restart: \"AFTER RESTART\" };"
        : "DSLTxt = { select: \"ВЫБРАТЬ\", selected: \"ВЫБРАНО\", have: \"ЕСТЬ У ВАС\", restart: \"ПОСЛЕ ПЕРЕЗАПУСКА\" };";
    g_OrigRunScript(engine, panel, texts, "dota_skins_lang.js", 0);
    g_OrigRunScript(engine, panel, kPickJs, "dota_skins_pick.js", 0);
    g_injectPanel = nullptr;
    t_inOurScript = false;
}

// mov [rsp+20],r9; mov [rsp+18],r8; mov [rsp+10],rdx; push rbp/rbx/rsi/rdi
static const uint8_t kRunScriptProlog[] = {
    0x4C, 0x89, 0x4C, 0x24, 0x20, 0x4C, 0x89, 0x44, 0x24, 0x18,
    0x48, 0x89, 0x54, 0x24, 0x10, 0x55, 0x53, 0x56, 0x57
};
static const size_t kRunScriptStolen = sizeof(kRunScriptProlog);  // 19 bytes, no RIP-relative

static uint8_t* FindRunScript(HMODULE mod) {
    static const char kName[] = "CUIEngine::RunScript (compile+run)";
    MODULEINFO mi{};
    if (!GetModuleInformation(GetCurrentProcess(), mod, &mi, sizeof(mi))) return nullptr;
    uint8_t* base = (uint8_t*)mi.lpBaseOfDll;
    size_t size = mi.SizeOfImage;
    uint8_t* str = nullptr;
    for (size_t i = 0; i + sizeof(kName) <= size; i++)
        if (memcmp(base + i, kName, sizeof(kName)) == 0) { str = base + i; break; }
    if (!str) return nullptr;
    // lea rcx,[rip+disp32] -> the string; then back to the prologue (the function is ~0x70 bytes before the lea)
    for (size_t i = 0; i + 7 <= size; i++) {
        uint8_t* p = base + i;
        if (p[0] != 0x48 || p[1] != 0x8D || p[2] != 0x0D) continue;
        if (p + 7 + *(int32_t*)(p + 3) != str) continue;
        for (size_t back = 0; back < 0x200 && p - back >= base; back++) {
            uint8_t* f = p - back;
            if (memcmp(f, kRunScriptProlog, sizeof(kRunScriptProlog)) == 0) return f;
        }
        return nullptr;
    }
    return nullptr;
}

// $.Msg from a script goes to tier0 (Msg / LoggingSystem_Log*): hook panorama.dll's
// imports and catch the DSL_* lines.
// --- Taunts (client-side) ---
// A taunt item is an activity modifier (ACT_DOTA_TAUNT + "loser") the game server
// plays on your hero when your inventory has the taunt. Here Dota's taunt key plays the preset's
// taunt on your own hero in this client only (nobody else sees it), the way the
// game plays a gesture from the server:
//  1. the taunt's modifier becomes the game's symbol (CUtlSymbolTable::AddString) and
//     joins the hero's own activity modifiers, so the game's sequence choice
//     (NPC vtable "select sequence") gives the taunt's own animation;
//  2. CDOTAAnimState::AddGesture puts it into a gesture slot;
//  3. gesture layers (CAnimationLayer) are networked: every frame the client takes
//     them from the server, which knows nothing of the taunt, so the layer would be
//     emptied at once. CDOTAAnimState::UpdateGestures (every frame, every gesture
//     slot) is hooked: before and after it the taunt is written back into its slot,
//     its cycle counted from the start, until the animation ends.
// Every address is found at run time from strings and code shapes in client.dll,
// so it survives game updates as long as that code keeps its shape.
using StartGestureFn = void (*)(void* entity, void* args);
using LookupActivityFn = int (*)(void* entity, const char* name);
using AddGestureFn = void (*)(void* state, int slot, int activity, bool flag, int sequence, float fadeIn, float fadeOut, float rate);
using SelectSeqFn = int* (*)(void* npc, int* out, int activity, const uint16_t* mods, int count);
using AddSymbolFn = uint16_t* (*)(void* table, uint16_t* out, const char* name, bool* created);
using UpdateGesturesFn = int (*)(void* state, void* arg);

static bool g_tauntResolved = false;
static void** g_entityList = nullptr;           // global holding the entity chunk array
static LookupActivityFn g_LookupActivity = nullptr;
static AddGestureFn g_AddGesture = nullptr;
static uint32_t g_animStateOffset = 0;          // C_DOTA_BaseNPC -> CDOTAAnimState*
static uint32_t g_selectSlot = 0;               // NPC vtable offset of the sequence choice
static uint32_t g_modsCount = 0, g_modsData = 0; // NPC's activity modifiers (CUtlVector<CUtlSymbol>)
static AddSymbolFn g_AddSymbol = nullptr;
static void* g_modSymbols = nullptr;            // the activity modifier CUtlSymbolTable
static UpdateGesturesFn g_OrigUpdateGestures = nullptr;
static bool g_updateHooked = false;
static int g_heroIndex = -1;
static std::string g_heroUnit;

static const int kTauntSlot = 2;                // the gesture slot the game uses for gestures
// the taunt playing now (read by the per-frame hook)
static void* volatile g_tauntState = nullptr;   // the hero's anim state while a taunt plays
static uint8_t* g_tauntLayer = nullptr;
static uint8_t g_tauntSnap[0x70];               // layer bytes +0x30..+0xA0 right after the start
static int g_tauntSeq = -1;
static ULONGLONG g_tauntStart = 0;
static float g_tauntCycleRate = 0.0f;          // cycles per second (m_flCachedSequenceCycleRate)
static float g_tauntPrevCycle = 0.0f;
static volatile LONG g_tauntRestores = 0;
// the taunt's modifier in the hero's own modifiers while it plays
static void* g_addedModEnt = nullptr;
static uint16_t g_addedMod = 0xFFFF;

// CAnimationLayer fields (from the schema): m_op (+0x30) holds m_hSequence +0x38,
// m_flPrevCycle +0x3c, m_flCycle +0x40, m_flWeight +0x44; m_nOrder +0x58,
// m_flCachedSequenceCycleRate +0x70, m_bSequenceFinished +0x78.
static void ApplyTaunt(void* state) {
    if (!g_tauntState || state != g_tauntState || !g_tauntLayer) return;
    double elapsed = (GetTickCount64() - g_tauntStart) / 1000.0;
    float cycle = (float)(elapsed * g_tauntCycleRate);
    if (cycle >= 1.0f || elapsed > 20.0) {
        g_tauntState = nullptr;
        return;
    }
    uint8_t* l = g_tauntLayer;
    if (*(int*)(l + 0x38) != g_tauntSeq || *(int*)(l + 0x58) != *(int*)(g_tauntSnap + 0x28)) {
        memcpy(l + 0x30, g_tauntSnap, sizeof(g_tauntSnap));
        InterlockedIncrement(&g_tauntRestores);
    }
    *(float*)(l + 0x3c) = g_tauntPrevCycle;
    *(float*)(l + 0x40) = cycle;
    *(float*)(l + 0x44) = 1.0f;
    *(uint8_t*)(l + 0x78) = 0;
    g_tauntPrevCycle = cycle;
}

static int HookedUpdateGestures(void* state, void* arg) {
    ApplyTaunt(state);
    int r = g_OrigUpdateGestures(state, arg);
    ApplyTaunt(state);
    return r;
}

static void RemoveAddedModifier() {
    if (!g_addedModEnt) return;
    int* count = (int*)((uint8_t*)g_addedModEnt + g_modsCount);
    uint16_t* data = *(uint16_t**)((uint8_t*)g_addedModEnt + g_modsData);
    if (data && *count > 0 && data[*count - 1] == g_addedMod) (*count)--;
    g_addedModEnt = nullptr;
    g_addedMod = 0xFFFF;
}

struct Section { uint8_t* start; size_t size; };

static bool ModuleSection(HMODULE mod, const char* name, Section& out) {
    uint8_t* base = (uint8_t*)mod;
    auto dos = (IMAGE_DOS_HEADER*)base;
    auto nt = (IMAGE_NT_HEADERS64*)(base + dos->e_lfanew);
    auto sec = IMAGE_FIRST_SECTION(nt);
    for (int i = 0; i < nt->FileHeader.NumberOfSections; i++, sec++) {
        if (strncmp((const char*)sec->Name, name, 8) == 0) {
            out.start = base + sec->VirtualAddress;
            out.size = sec->Misc.VirtualSize;
            return true;
        }
    }
    return false;
}

// A string in .rdata (a whole one, or one starting with `s` when `prefix`).
static uint8_t* FindString(const Section& rdata, const char* s, bool prefix = false) {
    size_t n = strlen(s) + (prefix ? 0 : 1);
    for (size_t i = 1; i + n <= rdata.size; i++)
        if (rdata.start[i - 1] == 0 && memcmp(rdata.start + i, s, n) == 0) return rdata.start + i;
    return nullptr;
}

// The first lea/mov reg,[rip+X] in .text with X == target.
static uint8_t* FindRipRef(const Section& text, const uint8_t* target) {
    if (!target) return nullptr;
    for (size_t i = 0; i + 7 <= text.size; i++) {
        uint8_t* p = text.start + i;
        if ((p[0] == 0x48 || p[0] == 0x4C) && (p[1] == 0x8D || p[1] == 0x8B) && (p[2] & 0xC7) == 0x05 &&
            p + 7 + *(int32_t*)(p + 3) == target)
            return p;
    }
    return nullptr;
}

// The first call rel32 to `target`.
static uint8_t* FindCallTo(const Section& text, const uint8_t* target) {
    if (!target) return nullptr;
    for (size_t i = 0; i + 5 <= text.size; i++) {
        uint8_t* p = text.start + i;
        if (p[0] == 0xE8 && p + 5 + *(int32_t*)(p + 1) == target) return p;
    }
    return nullptr;
}

// The start of the function holding `p` (back to its int3 padding).
static uint8_t* FunctionStart(uint8_t* p, size_t limit = 0x1000) {
    for (uint8_t* q = p; p && q > p - limit; q--)
        if (q[-1] == 0xCC && q[-2] == 0xCC && q[0] != 0xCC) return q;
    return nullptr;
}

// The target of the call that ends `pattern`, within `len` bytes from `from`.
static uint8_t* CallAfter(uint8_t* from, size_t len, const uint8_t* pattern, size_t n) {
    for (uint8_t* p = from; from && p < from + len; p++)
        if (memcmp(p, pattern, n) == 0) return p + n + 4 + *(int32_t*)(p + n);
    return nullptr;
}

static void ResolveTaunts() {
    g_tauntResolved = true;
    HMODULE cl = GetModuleHandleA("client.dll");
    Section text{}, rdata{};
    if (!cl || !ModuleSection(cl, ".text", text) || !ModuleSection(cl, ".rdata", rdata)) {
        Log("[-] Taunts: client.dll sections not found");
        return;
    }
    // The scene script binding StartGestureOverride: its impl looks activities up by
    // name (LookupActivity), and the binding reads the entity list at its start.
    uint8_t* bind = FindRipRef(text, FindString(rdata, "C_PortraitWorldUnit_API::StartGestureOverride"));
    static const uint8_t kImpl[] = {0x48, 0x8D, 0x54, 0x24, 0x20, 0x48, 0x8B, 0xCE, 0x48, 0x89, 0x44, 0x24, 0x28, 0xE8};
    uint8_t* impl = CallAfter(bind, 0xC0, kImpl, sizeof(kImpl));
    static const uint8_t kLookup[] = {0x48, 0x0F, 0x45, 0xD1, 0x48, 0x8B, 0xCF, 0xE8};
    g_LookupActivity = (LookupActivityFn)CallAfter(impl, 0x200, kLookup, sizeof(kLookup));
    for (uint8_t* p = bind; bind && p > bind - 0x200; p--) {
        if (p[0] == 0x4C && p[1] == 0x8B && p[2] == 0x15) { g_entityList = (void**)(p + 7 + *(int32_t*)(p + 3)); break; }
    }
    // CDOTAAnimState::AddToGestureSlot (assert string) -> its caller AddGesture; it
    // also calls the sequence choice with the NPC's modifiers.
    uint8_t* slotRef = FindRipRef(text, FindString(rdata, "CDOTAAnimState::AddToGestureSlot"));
    uint8_t* slotFn = nullptr;
    for (uint8_t* q = slotRef; slotRef && q > slotRef - 0x100; q--) {
        if (q[0] == 0x44 && q[1] == 0x88 && q[2] == 0x4C && q[3] == 0x24 && q[4] == 0x20) { slotFn = q; break; }
    }
    uint8_t* addCall = FindCallTo(text, slotFn);
    for (uint8_t* q = addCall; addCall && q > addCall - 0x600; q--) {
        if (q[0] == 0x44 && q[1] == 0x88 && q[2] == 0x4C && q[3] == 0x24 && q[4] == 0x20 && q[-1] == 0xCC) {
            g_AddGesture = (AddGestureFn)q;
            break;
        }
    }
    static const uint8_t kSel[] = {0x48, 0x8D, 0x94, 0x24, 0xA8, 0x00, 0x00, 0x00, 0x48, 0x8B, 0xCE, 0xE8};
    uint8_t* sel = CallAfter(slotFn, 0x200, kSel, sizeof(kSel));
    for (uint8_t* q = sel; sel && q < sel + 0x40; q++) {
        if (q[0] == 0x8B && q[1] == 0x81) g_modsCount = *(uint32_t*)(q + 2);
        if (q[0] == 0x4C && q[1] == 0x8B && q[2] == 0x89) g_modsData = *(uint32_t*)(q + 3);
        if (q[0] == 0x41 && q[1] == 0xFF && q[2] == 0x92) { g_selectSlot = *(uint32_t*)(q + 3); break; }
    }
    // the anim state's offset in the NPC: mov rcx,[rcx+X] right before calls to AddGesture
    for (size_t i = 0, votes = 0; g_AddGesture && i + 5 <= text.size && votes < 3; i++) {
        uint8_t* p = text.start + i;
        if (p[0] != 0xE8 || p + 5 + *(int32_t*)(p + 1) != (uint8_t*)g_AddGesture) continue;
        for (uint8_t* q = p - 7; q > p - 0x40; q--) {
            if (q[0] == 0x48 && q[1] == 0x8B && (q[2] == 0x89 || q[2] == 0x88)) {
                uint32_t off = *(uint32_t*)(q + 3);
                if (off > 0x100 && off < 0x10000) {
                    if (!g_animStateOffset || g_animStateOffset == off) { g_animStateOffset = off; votes++; }
                    break;
                }
            }
        }
    }
    // the modifier symbol table: lea rcx,<table> right before a call to tier0's AddString
    if (HMODULE t0 = GetModuleHandleA("tier0.dll"))
        g_AddSymbol = (AddSymbolFn)GetProcAddress(t0, "?AddString@CUtlSymbolTable@@QEAA?AVCUtlSymbol@@PEBDPEA_N@Z");
    static const uint8_t kTab[] = {0x45, 0x33, 0xC9, 0x48, 0x8D, 0x54, 0x24, 0x24, 0x4C, 0x8B, 0xC6, 0x48, 0x8D, 0x0D};
    for (size_t i = 0; g_AddSymbol && i + sizeof(kTab) + 10 <= text.size; i++) {
        uint8_t* p = text.start + i;
        if (memcmp(p, kTab, sizeof(kTab)) != 0 || p[sizeof(kTab) + 4] != 0xFF || p[sizeof(kTab) + 5] != 0x15) continue;
        uint8_t* next = p + sizeof(kTab) + 4;
        if (*(void**)(next + 6 + *(int32_t*)(next + 2)) == (void*)g_AddSymbol) {
            g_modSymbols = next + *(int32_t*)(p + sizeof(kTab));
            break;
        }
    }
    // UpdateGestureLayer (log string) -> its only caller, UpdateGestures: hooked
    uint8_t* layerFn = FunctionStart(FindRipRef(text, FindString(rdata, "CDOTAAnimState::UpdateGestureLayer layer %d", true)));
    uint8_t* gestures = FunctionStart(FindCallTo(text, layerFn));
    // mov rax,rsp; mov [rax+10h],rdx; push rsi; sub rsp,70h; mov [rax+18h],rbx (16 bytes, no RIP-relative)
    static const uint8_t kProlog[] = {0x48, 0x8B, 0xC4, 0x48, 0x89, 0x50, 0x10, 0x56,
                                      0x48, 0x83, 0xEC, 0x70, 0x48, 0x89, 0x58, 0x18};
    if (gestures && memcmp(gestures, kProlog, sizeof(kProlog)) == 0 &&
        InstallHook(gestures, sizeof(kProlog), (void*)&HookedUpdateGestures, (void**)&g_OrigUpdateGestures))
        g_updateHooked = true;

    bool ok = g_entityList && g_LookupActivity && g_AddGesture && g_animStateOffset && g_selectSlot &&
              g_modsCount && g_modsData && g_AddSymbol && g_modSymbols && g_updateHooked;
    Log("[%c] Taunts: entity list %p, LookupActivity %p, AddGesture %p, anim state +0x%X, select +0x%X, "
        "modifiers +0x%X/+0x%X, symbols %p, UpdateGestures %s",
        ok ? '+' : '-', (void*)g_entityList, (void*)g_LookupActivity, (void*)g_AddGesture, g_animStateOffset,
        g_selectSlot, g_modsCount, g_modsData, g_modSymbols, g_updateHooked ? "hooked" : "NOT hooked");
}

// Entity by index: chunks of 512 entries of 0x70 bytes, entry+0 = entity, entry+0x10 = handle.
static void* EntityByIndex(int index) {
    if (!g_entityList || index < 0) return nullptr;
    uint8_t** chunks = (uint8_t**)*g_entityList;
    if (!chunks) return nullptr;
    int c = index >> 9;
    if (c > 0x3f || !chunks[c]) return nullptr;
    uint8_t* e = chunks[c] + (index & 0x1ff) * 0x70;
    if ((*(uint32_t*)(e + 0x10) & 0x7fff) != (uint32_t)index) return nullptr;
    return *(void**)e;
}

// --- Item models in the world (the glows hang on them, as the game hangs them) ---
// An item's effect is created by the game on the item's own model (its wearable):
// a particle skinning a model snapshot to the item's bones (Twilight Schism's blade
// outline: `blade_02_JNT`) needs that skeleton. The DLL finds the wearables whose
// model is a glow's item model (or a model the swap table turns into it) and gives
// the interface script their entity indices with the hero they hang on.
//
// Field offsets come from the schema data client.dll carries for its classes, so they
// follow game updates: a class description is {name, …, size (+0x18, int32), field
// count (+0x1C, int16), …, fields (+0x28)}, a field {name, type, offset (+0x10)} in
// 0x20 bytes.
struct ItemHostOffsets {
    int body = -1, sceneNode = -1, skeleton = -1, modelState = -1, modelName = -1, modelHandle = -1;
    int parent = -1, owner = -1, entity = -1, ownerEntity = -1;
};
static ItemHostOffsets g_ih;
static volatile LONG g_itemHostsReady = 0;

struct SchemaField { const char* cls; const char* field; int* out; };

static void ReadSchemaOffsets(const Section& rdata, const Section& data, SchemaField* want, size_t n) {
    const uint8_t *rlo = rdata.start, *rhi = rdata.start + rdata.size;
    const uint8_t *dlo = data.start, *dhi = data.start + data.size;
    for (size_t i = 0; i + 0x30 <= data.size; i += 8) {
        const uint8_t* h = data.start + i;
        const uint8_t* name = *(const uint8_t* const*)h;
        if (name < rlo || name >= rhi) continue;
        int32_t size = *(const int32_t*)(h + 0x18);
        uint16_t count = *(const uint16_t*)(h + 0x1C);
        const uint8_t* fields = *(const uint8_t* const*)(h + 0x28);
        if (size <= 0 || size > 0x100000 || count == 0 || count > 4096) continue;
        if (fields < dlo || fields + (size_t)count * 0x20 > dhi) continue;
        for (size_t w = 0; w < n; w++) {
            if (*want[w].out >= 0 || strcmp((const char*)name, want[w].cls) != 0) continue;
            for (uint16_t f = 0; f < count; f++) {
                const uint8_t* fname = *(const uint8_t* const*)(fields + f * 0x20);
                if (fname >= rlo && fname < rhi && strcmp((const char*)fname, want[w].field) == 0) {
                    *want[w].out = *(const int32_t*)(fields + f * 0x20 + 0x10);
                    break;
                }
            }
        }
    }
}

// Off the interface thread: the scans take a moment and client.dll may still be loading.
static DWORD WINAPI ResolveItemHosts(LPVOID) {
    HMODULE cl = nullptr;
    for (int i = 0; i < 6000 && !(cl = GetModuleHandleA("client.dll")); i++) Sleep(100);
    Section text{}, rdata{}, data{};
    if (!cl || !ModuleSection(cl, ".text", text) || !ModuleSection(cl, ".rdata", rdata) || !ModuleSection(cl, ".data", data)) {
        Log("[-] Item models: client.dll sections not found");
        return 1;
    }
    ItemHostOffsets o;
    SchemaField want[] = {
        {"C_BaseEntity", "m_CBodyComponent", &o.body},
        {"C_BaseEntity", "m_hOwnerEntity", &o.ownerEntity},
        {"CBodyComponent", "m_pSceneNode", &o.sceneNode},
        {"CBodyComponentSkeletonInstance", "m_skeletonInstance", &o.skeleton},
        {"CSkeletonInstance", "m_modelState", &o.modelState},
        {"CModelState", "m_ModelName", &o.modelName},
        {"CModelState", "m_hModel", &o.modelHandle},
        {"CGameSceneNode", "m_pParent", &o.parent},
        {"CGameSceneNode", "m_pOwner", &o.owner},
        {"CEntityInstance", "m_pEntity", &o.entity},
    };
    ReadSchemaOffsets(rdata, data, want, sizeof(want) / sizeof(want[0]));
    // the entity chunk list, as the taunts find it
    void** list = g_entityList;
    uint8_t* bind = list ? nullptr : FindRipRef(text, FindString(rdata, "C_PortraitWorldUnit_API::StartGestureOverride"));
    for (uint8_t* p = bind; bind && p > bind - 0x200; p--) {
        if (p[0] == 0x4C && p[1] == 0x8B && p[2] == 0x15) { list = (void**)(p + 7 + *(int32_t*)(p + 3)); break; }
    }
    bool ok = list && o.body >= 0 && o.ownerEntity >= 0 && o.sceneNode >= 0 && o.skeleton >= 0 && o.modelState >= 0 &&
              o.modelName >= 0 && o.parent >= 0 && o.owner >= 0 && o.entity >= 0;
    Log("[%c] Item models: entity list %p, body +0x%X, scene node +0x%X, skeleton +0x%X, model state +0x%X, "
        "model name +0x%X, model +0x%X, parent +0x%X, node owner +0x%X, identity +0x%X, owner entity +0x%X",
        ok ? '+' : '-', (void*)list, o.body, o.sceneNode, o.skeleton, o.modelState, o.modelName, o.modelHandle,
        o.parent, o.owner, o.entity, o.ownerEntity);
    if (!ok) return 1;
    g_ih = o;
    if (!g_entityList) g_entityList = list;
    InterlockedExchange(&g_itemHostsReady, 1);
    return 0;
}

static int EntityIndexOf(const uint8_t* ent) {
    if (!ent) return -1;
    const uint8_t* id = *(const uint8_t* const*)(ent + g_ih.entity);
    return id ? (int)(*(const uint32_t*)(id + 0x10) & 0x7fff) : -1;
}

// Whether an object with a vtable is a `cls` (MSVC RTTI type name, ".?AVCSkeletonInstance@@")
// or derives from it: the complete object locator before its vtable lists the class and
// all its bases (x64: RVAs from the module base, which the locator gives as its own RVA).
// Answers are kept per vtable: there are few.
static bool IsA(const void* obj, const char* cls, std::unordered_map<const void*, bool>& seen) {
    if (!obj || IsBadReadPtr(obj, 8)) return false;
    const uint8_t* vt = *(const uint8_t* const*)obj;
    auto it = seen.find(vt);
    if (it != seen.end()) return it->second;
    bool is = false;
    const uint8_t* col = (vt && !IsBadReadPtr(vt - 8, 8)) ? *(const uint8_t* const*)(vt - 8) : nullptr;
    if (col && !IsBadReadPtr(col, 0x18) && *(const uint32_t*)col == 1) {
        const uint8_t* base = col - *(const int32_t*)(col + 0x14);
        const uint8_t* hier = base + *(const int32_t*)(col + 0x10);
        if (!IsBadReadPtr(hier, 0x10)) {
            uint32_t n = *(const uint32_t*)(hier + 8);
            const int32_t* bases = (const int32_t*)(base + *(const int32_t*)(hier + 0xC));
            for (uint32_t i = 0; i < n && i < 64 && !IsBadReadPtr(bases + i, 4); i++) {
                const uint8_t* bcd = base + bases[i];
                if (IsBadReadPtr(bcd, 4)) break;
                const char* name = (const char*)(base + *(const int32_t*)bcd + 0x10);
                if (!IsBadReadPtr(name, 1) && strcmp(name, cls) == 0) { is = true; break; }
            }
        }
    }
    seen.emplace(vt, is);
    return is;
}

// The model of a model entity, lower case without "_c"; empty for others. Its scene node
// must be a skeleton (CSkeletonInstance): a point entity's plain scene node sits at the
// same place in its body component, and is smaller (reading the model state past its
// end crashed the game).
static std::string ModelOf(const uint8_t* ent, const uint8_t** nodeOut) {
    static std::unordered_map<const void*, bool> entities, skeletons;
    if (!IsA(ent, ".?AVC_BaseEntity@@", entities)) return std::string();
    const uint8_t* body = *(const uint8_t* const*)(ent + g_ih.body);
    if (!body || IsBadReadPtr(body, g_ih.sceneNode + 8)) return std::string();
    const uint8_t* node = *(const uint8_t* const*)(body + g_ih.sceneNode);
    if (!node || node != body + g_ih.skeleton || !IsA(node, ".?AVCSkeletonInstance@@", skeletons)) return std::string();
    *nodeOut = node;
    // (no fallback to the model handle's resource name: asking the resource system
    // for it with that handle crashed the game)
    const char* name = *(const char* const*)(node + g_ih.modelState + g_ih.modelName);
    if (!name || IsBadReadPtr(name, 1)) return std::string();
    std::string model = Normalize(std::string(name, strnlen(name, 260)));
    if (model.size() > 2 && model.compare(model.size() - 2, 2, "_c") == 0) model.resize(model.size() - 2);
    return model;
}

// The item models of the glows in the world, as the script's table:
// {"item model":[[entity,handle,parent,owner],…],…}
static std::string ScanItemHosts() {
    std::unordered_map<std::string, std::string> wanted;
    AcquireSRWLockShared(&g_glowsLock);
    wanted = g_itemModels;
    ReleaseSRWLockShared(&g_glowsLock);
    std::map<std::string, std::string> found;  // item model -> "[entity,handle,parent,owner],…"
    std::string attached;                      // models on other entities (for the log)
    int attachedCount = 0;
    uint8_t** chunks = wanted.empty() ? nullptr : (uint8_t**)*g_entityList;
    for (int c = 0; chunks && c < 64; c++) {
        uint8_t* chunk = chunks[c];
        if (!chunk) continue;
        for (int i = 0; i < 512; i++) {
            uint8_t* e = chunk + i * 0x70;
            const uint8_t* ent = *(const uint8_t* const*)e;
            uint32_t handle = *(const uint32_t*)(e + 0x10);
            int index = c * 512 + i;
            if (!ent || (int)(handle & 0x7fff) != index) continue;
            const uint8_t* node = nullptr;
            std::string model = ModelOf(ent, &node);
            if (model.empty()) continue;
            // the hero it hangs on: its scene node's parent (a wearable follows the
            // hero's bones), and its owner entity
            const uint8_t* parentNode = *(const uint8_t* const*)(node + g_ih.parent);
            auto it = wanted.find(model);
            if (it == wanted.end()) {
                if (parentNode && attachedCount++ < 40) attached += " " + std::to_string(index) + ":" + model;
                continue;
            }
            int parent = parentNode ? EntityIndexOf(*(const uint8_t* const*)(parentNode + g_ih.owner)) : -1;
            uint32_t ownerHandle = *(const uint32_t*)(ent + g_ih.ownerEntity);
            int owner = ownerHandle == 0xFFFFFFFF ? -1 : (int)(ownerHandle & 0x7fff);
            std::string& s = found[it->second];
            s += (s.empty() ? "" : ",") + ("[" + std::to_string(index) + "," + std::to_string(handle) + "," +
                                          std::to_string(parent) + "," + std::to_string(owner) + "]");
        }
    }
    // no item model in the world while models hang on heroes: say which ones (once per
    // glow table), so a name the swap table does not know shows in the log
    static LONG diagVersion = -1;
    if (found.empty() && attachedCount > 0 && diagVersion != g_glowsVersion) {
        diagVersion = g_glowsVersion;
        Log("[*] Item models for glows: none of %zu wanted; attached models:%.1500s", wanted.size(), attached.c_str());
    }
    std::string js = "{", summary;
    for (auto& f : found) {
        js += (js.size() > 1 ? "," : "") + JsString(f.first) + ":[" + f.second + "]";
        size_t n = std::count(f.second.begin(), f.second.end(), '[');
        summary += " " + f.first.substr(f.first.rfind('/') + 1) + " x" + std::to_string(n);
    }
    js += "}";
    // the wardrobe's cards re-create their models all the time: the log says what is
    // found when that changes, at most every 5 s
    static std::string lastSummary;
    static ULONGLONG lastLog = 0;
    if (summary != lastSummary && GetTickCount64() - lastLog >= 5000) {
        lastSummary = summary;
        lastLog = GetTickCount64();
        Log("[*] Item models for glows:%s", summary.empty() ? " none" : summary.c_str());
    }
    return js;
}

// Interface thread, from a loop's tick (right before its DSLGlowTick): the item models of
// the glows -> DSLSetItemHosts of that loop, when they change. One scan serves both loops.
static std::string g_itemHostsSent[2] = {"{}", "{}"};
static void MaybeSendItemHosts(int kind) {
    void* panel = g_loopPanels[kind];
    if (!g_itemHostsReady || !g_entityList || !g_uiEngine || !panel || !g_OrigRunScript) return;
    static std::string js = "{}";
    static ULONGLONG lastScan = 0;
    ULONGLONG now = GetTickCount64();
    if (now - lastScan >= 200) {
        js = ScanItemHosts();
        lastScan = now;
    }
    if (js == g_itemHostsSent[kind]) return;
    g_itemHostsSent[kind] = js;
    std::string script = "if (typeof DSLSetItemHosts === 'function') DSLSetItemHosts(" + js + ");";
    // a new name every time and per loop (Panorama keeps a compiled script by its name)
    static LONG version = 0;
    std::string name = std::string("dota_skins_hosts_") + kLoopNames[kind] + "_" + std::to_string(++version) + ".js";
    t_inOurScript = true;
    g_OrigRunScript(g_uiEngine, panel, script.c_str(), name.c_str(), 0);
    t_inOurScript = false;
}

// "index|unit name" from the interface script
static void SetLocalHero(const char* v) {
    g_heroIndex = atoi(v);
    const char* bar = strchr(v, '|');
    g_heroUnit = bar ? std::string(bar + 1) : std::string();
    while (!g_heroUnit.empty() && (g_heroUnit.back() == '\n' || g_heroUnit.back() == '\r' || g_heroUnit.back() == ' '))
        g_heroUnit.pop_back();
    Log("[*] Your hero: %s (entity %d)", g_heroUnit.c_str(), g_heroIndex);
}

static void StartTaunt() {
    std::string cfg;
    AcquireSRWLockShared(&g_tauntLock);
    auto it = g_taunts.find(g_heroUnit);
    if (it != g_taunts.end()) cfg = it->second;
    ReleaseSRWLockShared(&g_tauntLock);
    if (cfg.empty()) { Log("[*] Taunt: no taunt in the preset for %s", g_heroUnit.c_str()); return; }
    // "name#index;name#index@modifier@ACTIVITY" (the activity defaults to ACT_DOTA_TAUNT)
    std::string seqs = cfg, modifier, activityName = "ACT_DOTA_TAUNT";
    size_t at = cfg.find('@');
    if (at != std::string::npos) {
        seqs = cfg.substr(0, at);
        modifier = cfg.substr(at + 1);
        size_t at2 = modifier.find('@');
        if (at2 != std::string::npos) {
            activityName = modifier.substr(at2 + 1);
            modifier = modifier.substr(0, at2);
        }
    }
    size_t hash = seqs.find('#');
    int fallback = hash == std::string::npos ? -1 : atoi(seqs.c_str() + hash + 1);

    void* ent = EntityByIndex(g_heroIndex);
    if (!ent) { Log("[-] Taunt: hero entity %d not found", g_heroIndex); return; }
    void* state = *(void**)((uint8_t*)ent + g_animStateOffset);
    if (!state) return;
    int activity = g_LookupActivity(ent, activityName.c_str());
    if (activity < 0) { Log("[-] Taunt: %s has no %s", g_heroUnit.c_str(), activityName.c_str()); return; }

    // the taunt's modifier joins the hero's own: the game then chooses the taunt's sequence
    uint16_t sym = 0xFFFF;
    if (!modifier.empty()) g_AddSymbol(g_modSymbols, &sym, modifier.c_str(), nullptr);
    int* cnt = (int*)((uint8_t*)ent + g_modsCount);
    uint16_t* data = *(uint16_t**)((uint8_t*)ent + g_modsData);
    int alloc = *(int*)((uint8_t*)ent + g_modsData + 8);
    uint32_t flags = *(uint32_t*)((uint8_t*)ent + g_modsData + 12);
    std::vector<uint16_t> mods;
    for (int k = 0; data && k < *cnt && k < 32; k++) mods.push_back(data[k]);
    if (sym != 0xFFFF) mods.push_back(sym);
    auto select = (SelectSeqFn)(*(void***)ent)[g_selectSlot / 8];
    int seq = -1;
    select(ent, &seq, activity, mods.empty() ? nullptr : mods.data(), (int)mods.size());
    if (seq < 0) seq = fallback;
    if (seq < 0) { Log("[-] Taunt: no sequence for %s/%s on %s", activityName.c_str(), modifier.c_str(), g_heroUnit.c_str()); return; }
    if (sym != 0xFFFF && data && *cnt < alloc && !(flags & 0x40000000)) {
        data[*cnt] = sym;
        (*cnt)++;
        g_addedModEnt = ent;
        g_addedMod = sym;
    }

    uint8_t* layers = *(uint8_t**)((uint8_t*)state + 8);
    uint8_t* layer = layers ? *(uint8_t**)(layers + kTauntSlot * 0x18 + 8) : nullptr;
    g_AddGesture(state, kTauntSlot, activity, true, seq, 0.0f, 0.1f, 1.0f);
    if (!layer || *(int*)(layer + 0x38) != seq) { Log("[-] Taunt: the game did not take sequence %d", seq); return; }
    memcpy(g_tauntSnap, layer + 0x30, sizeof(g_tauntSnap));
    g_tauntLayer = layer;
    g_tauntSeq = seq;
    g_tauntCycleRate = *(float*)(layer + 0x70);
    if (!(g_tauntCycleRate > 0.01f && g_tauntCycleRate < 10.0f)) g_tauntCycleRate = 0.25f;
    g_tauntPrevCycle = 0.0f;
    g_tauntStart = GetTickCount64();
    g_tauntRestores = 0;
    g_tauntState = state;
    Log("[*] Taunt: %s on %s (%s %s, sequence %d, %.1f s)", seqs.c_str(), g_heroUnit.c_str(), activityName.c_str(),
        modifier.c_str(), seq, 1.0f / g_tauntCycleRate);
}

// --- The taunt key: Dota's own "HeroTaunt" binding ---
// Steam\userdata\<account>\570\remote\cfg\dotakeys_personal.lst, block
// "HeroTaunt" { "Key" "N" "Modifier" "ALT" }; the account is Steam's active user
// (else the newest such file). Re-read when the file changes, so a key changed in
// Dota's settings is picked up in the running game. No usable binding: F8.
static int g_tauntVk = VK_F8;
static int g_tauntMods = 0;  // 1 shift, 2 ctrl, 4 alt
static std::string g_keysFile;
static FILETIME g_keysTime{};
static ULONGLONG g_keysChecked = 0;

static int VkFromDotaKey(std::string k) {
    if (k.empty()) return 0;
    if (k.size() == 1) {
        char c = k[0];
        if (c >= 'a' && c <= 'z') return c - 'a' + 'A';
        if ((c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')) return c;
        switch (c) {
            case '`': return VK_OEM_3;
            case '-': return VK_OEM_MINUS;
            case '=': return VK_OEM_PLUS;
            case '[': return VK_OEM_4;
            case ']': return VK_OEM_6;
            case '\\': return VK_OEM_5;
            case ';': return VK_OEM_1;
            case '\'': return VK_OEM_7;
            case ',': return VK_OEM_COMMA;
            case '.': return VK_OEM_PERIOD;
            case '/': return VK_OEM_2;
        }
        return 0;
    }
    for (auto& ch : k) ch = (char)toupper((unsigned char)ch);
    if (k[0] == 'F' && k.size() <= 3 && isdigit((unsigned char)k[1])) {
        int n = atoi(k.c_str() + 1);
        if (n >= 1 && n <= 24) return VK_F1 + n - 1;
    }
    if (k.compare(0, 3, "KP_") == 0 && k.size() == 4 && isdigit((unsigned char)k[3])) return VK_NUMPAD0 + (k[3] - '0');
    static const struct { const char* name; int vk; } kNames[] = {
        {"SPACE", VK_SPACE}, {"TAB", VK_TAB}, {"ENTER", VK_RETURN}, {"ESCAPE", VK_ESCAPE},
        {"BACKSPACE", VK_BACK}, {"CAPSLOCK", VK_CAPITAL}, {"INS", VK_INSERT}, {"DEL", VK_DELETE},
        {"HOME", VK_HOME}, {"END", VK_END}, {"PGUP", VK_PRIOR}, {"PGDN", VK_NEXT},
        {"UPARROW", VK_UP}, {"DOWNARROW", VK_DOWN}, {"LEFTARROW", VK_LEFT}, {"RIGHTARROW", VK_RIGHT},
        {"MOUSE1", VK_LBUTTON}, {"MOUSE2", VK_RBUTTON}, {"MOUSE3", VK_MBUTTON},
        {"MOUSE4", VK_XBUTTON1}, {"MOUSE5", VK_XBUTTON2},
        {"KP_ENTER", VK_RETURN}, {"KP_PLUS", VK_ADD}, {"KP_MINUS", VK_SUBTRACT},
        {"KP_MULTIPLY", VK_MULTIPLY}, {"KP_SLASH", VK_DIVIDE}, {"KP_DEL", VK_DECIMAL},
        {"SHIFT", VK_SHIFT}, {"CTRL", VK_CONTROL}, {"ALT", VK_MENU},
        {"SEMICOLON", VK_OEM_1}, {"APOSTROPHE", VK_OEM_7}, {"COMMA", VK_OEM_COMMA},
        {"PERIOD", VK_OEM_PERIOD}, {"SLASH", VK_OEM_2}, {"BACKSLASH", VK_OEM_5},
        {"MINUS", VK_OEM_MINUS}, {"EQUALS", VK_OEM_PLUS}, {"LBRACKET", VK_OEM_4}, {"RBRACKET", VK_OEM_6},
    };
    for (auto& n : kNames)
        if (k == n.name) return n.vk;
    return 0;  // MWHEELUP/MWHEELDOWN and the like: not a key state
}

static std::string ReadRegString(HKEY root, const char* path, const char* name) {
    char buf[MAX_PATH] = {};
    DWORD size = sizeof(buf);
    if (RegGetValueA(root, path, name, RRF_RT_REG_SZ, nullptr, buf, &size) != ERROR_SUCCESS) return std::string();
    return buf;
}

static std::string FindKeysFile() {
    std::string steam = ReadRegString(HKEY_CURRENT_USER, "Software\\Valve\\Steam", "SteamPath");
    if (steam.empty()) return std::string();
    DWORD user = 0, size = sizeof(user);
    RegGetValueA(HKEY_CURRENT_USER, "Software\\Valve\\Steam\\ActiveProcess", "ActiveUser", RRF_RT_REG_DWORD, nullptr, &user, &size);
    const char* tail = "\\570\\remote\\cfg\\dotakeys_personal.lst";
    if (user) {
        std::string f = steam + "\\userdata\\" + std::to_string(user) + tail;
        if (GetFileAttributesA(f.c_str()) != INVALID_FILE_ATTRIBUTES) return f;
    }
    // no active user: the newest bindings of any account
    std::string best;
    FILETIME bestTime{};
    WIN32_FIND_DATAA fd;
    HANDLE h = FindFirstFileA((steam + "\\userdata\\*").c_str(), &fd);
    if (h == INVALID_HANDLE_VALUE) return std::string();
    do {
        if (!(fd.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY) || fd.cFileName[0] == '.') continue;
        std::string f = steam + "\\userdata\\" + fd.cFileName + tail;
        FILETIME t;
        if (FileTime(f, t) && CompareFileTime(&t, &bestTime) > 0) { bestTime = t; best = f; }
    } while (FindNextFileA(h, &fd));
    FindClose(h);
    return best;
}

// The value of "name" inside the block text.
static std::string KvValue(const std::string& block, const char* name) {
    std::string key = std::string("\"") + name + "\"";
    size_t at = block.find(key);
    if (at == std::string::npos) return std::string();
    size_t q1 = block.find('"', at + key.size());
    size_t q2 = q1 == std::string::npos ? q1 : block.find('"', q1 + 1);
    return q2 == std::string::npos ? std::string() : block.substr(q1 + 1, q2 - q1 - 1);
}

static void RefreshTauntKey() {
    ULONGLONG now = GetTickCount64();
    if (g_keysChecked && now - g_keysChecked < 3000) return;
    g_keysChecked = now;
    if (g_keysFile.empty()) g_keysFile = FindKeysFile();
    FILETIME t;
    if (g_keysFile.empty() || !FileTime(g_keysFile, t) || CompareFileTime(&t, &g_keysTime) == 0) return;
    g_keysTime = t;
    std::ifstream f(g_keysFile, std::ios::binary);
    std::string text((std::istreambuf_iterator<char>(f)), std::istreambuf_iterator<char>());
    int vk = VK_F8, mods = 0;
    std::string keyName = "F8 (no binding)";
    size_t at = text.find("\"HeroTaunt\"");
    size_t open = at == std::string::npos ? at : text.find('{', at);
    size_t close = open == std::string::npos ? open : text.find('}', open);
    if (close != std::string::npos) {
        std::string block = text.substr(open, close - open);
        std::string key = KvValue(block, "Key"), mod = KvValue(block, "Modifier");
        for (auto& ch : mod) ch = (char)toupper((unsigned char)ch);
        if (int v = VkFromDotaKey(key)) {
            vk = v;
            keyName = key;
            if (mod.find("SHIFT") != std::string::npos) mods |= 1;
            if (mod.find("CTRL") != std::string::npos) mods |= 2;
            if (mod.find("ALT") != std::string::npos) mods |= 4;
            if (mods) keyName = mod + "+" + key;
        } else if (!key.empty()) {
            keyName = "F8 (\"" + key + "\" is not a key)";
        }
    }
    g_tauntVk = vk;
    g_tauntMods = mods;
    Log("[*] Taunt key: %s (from %s)", keyName.c_str(), g_keysFile.c_str());
}

static bool TauntKeyDown() {
    auto held = [](int vk) { return (GetAsyncKeyState(vk) & 0x8000) != 0; };
    if (!held(g_tauntVk)) return false;
    if ((g_tauntMods & 1) && !held(VK_SHIFT)) return false;
    if ((g_tauntMods & 2) && !held(VK_CONTROL)) return false;
    if ((g_tauntMods & 4) && !held(VK_MENU)) return false;
    return true;
}

// Interface thread, every tick in a match: Dota's taunt key (while Dota is the
// active window) plays the hero's taunt; the modifier comes out again when it is over.
static void MaybeTaunt() {
    if (!g_tauntState && g_addedModEnt) {
        RemoveAddedModifier();
        Log("[*] Taunt: finished (layer put back %ld times)", g_tauntRestores);
    }
    RefreshTauntKey();
    static bool wasDown = false;
    bool down = TauntKeyDown();
    bool pressed = down && !wasDown;
    wasDown = down;
    if (!pressed) return;
    DWORD pid = 0;
    GetWindowThreadProcessId(GetForegroundWindow(), &pid);
    if (pid != GetCurrentProcessId()) return;
    if (!g_tauntResolved) ResolveTaunts();
    if (!g_updateHooked || !g_entityList || !g_LookupActivity || !g_AddGesture || !g_animStateOffset ||
        !g_selectSlot || !g_modsCount || !g_modsData || !g_AddSymbol || !g_modSymbols)
        return;
    // a taunt playing already: from the start again
    g_tauntState = nullptr;
    RemoveAddedModifier();
    StartTaunt();
}

static bool CheckDslMessage(const char* msg) {
    if (!msg) return false;
    const char* p = strstr(msg, "DSL_");
    if (!p) {
        if (strstr(msg, "SetPreview")) Log("[ui] %.600s", msg);
        return false;
    }
    // ticks and loop starts name their loop: "dash" (the menu's) or "hud" (the match's)
    int kind = strstr(p, " hud") ? kHud : kDash;
    if (strncmp(p, "DSL_TICK", 8) == 0) {
        if (g_inMenu) {
            RunPreloads();
            if (kind == kDash) MaybeRefreshPreview();
            MaybeSendGlows(kind);
        } else {
            MaybeTaunt();
        }
    }
    else if (strncmp(p, "DSL_HERO ", 9) == 0) SetLocalHero(p + 9);
    else if (strncmp(p, "DSL_GTICK", 9) == 0) { MaybeSendGlows(kind); MaybeSendItemHosts(kind); }
    else if (strncmp(p, "DSL_LOOP", 8) == 0) {
        // a new script context: it has neither the glow table nor the item models yet
        if (g_injectPanel) {
            g_loopPanels[kind] = g_injectPanel;
            g_glowsSent[kind] = -1;
            g_itemHostsSent[kind] = "{}";
        }
    }
    else if (strncmp(p, "DSL_PICK", 8) == 0) RecordPick(p[8] == ' ' ? p + 9 : nullptr);
    else if (strncmp(p, "DSL_MENU", 8) == 0) {
        LONG v = p[9] == '1';
        if (InterlockedExchange(&g_inMenu, v) != v) {
            Log("[*] %s", v ? "In menu: renaming swapped models" : "In game: file swap only");
        }
    }
    else if (strncmp(p, "DSL_LOG", 7) == 0) Log("[js] %.3800s", p + 7);
    else return false;
    return true;
}

using Var8Fn = uint64_t (*)(uint64_t, uint64_t, uint64_t, uint64_t, uint64_t, uint64_t, uint64_t, uint64_t);
static Var8Fn g_OrigMsg = nullptr, g_OrigLog = nullptr, g_OrigLogDirect = nullptr;

// The message from its format and arguments (up to 7 integers/pointers, as they came in registers/stack)
static void FormatInto(char* buf, size_t n, const char* fmt, uint64_t a, uint64_t b, uint64_t c, uint64_t d, uint64_t e) {
    buf[0] = 0;
    if (!fmt || IsBadReadPtr(fmt, 1)) return;
    if (!strstr(fmt, "%")) { snprintf(buf, n, "%s", fmt); return; }
    if (strcmp(fmt, "%s") == 0 || strcmp(fmt, "%s\n") == 0) {
        const char* s = (const char*)a;
        if (s && !IsBadReadPtr(s, 1)) snprintf(buf, n, "%s", s);
        return;
    }
    snprintf(buf, n, fmt, a, b, c, d, e);
}

static uint64_t HookedMsg(uint64_t a0, uint64_t a1, uint64_t a2, uint64_t a3, uint64_t a4, uint64_t a5, uint64_t a6, uint64_t a7) {
    char buf[4096];
    FormatInto(buf, sizeof(buf), (const char*)a0, a1, a2, a3, a4, a5);
    if (CheckDslMessage(buf)) return 0;
    return g_OrigMsg(a0, a1, a2, a3, a4, a5, a6, a7);
}

static uint64_t HookedLog(uint64_t a0, uint64_t a1, uint64_t a2, uint64_t a3, uint64_t a4, uint64_t a5, uint64_t a6, uint64_t a7) {
    char buf[4096];
    FormatInto(buf, sizeof(buf), (const char*)a2, a3, a4, a5, a6, a7);
    if (CheckDslMessage(buf)) return 0;
    return g_OrigLog(a0, a1, a2, a3, a4, a5, a6, a7);
}

static uint64_t HookedLogDirect(uint64_t a0, uint64_t a1, uint64_t a2, uint64_t a3, uint64_t a4, uint64_t a5, uint64_t a6, uint64_t a7) {
    const char* m = (const char*)a3;
    if (m && !IsBadReadPtr(m, 1) && CheckDslMessage(m)) return 0;
    return g_OrigLogDirect(a0, a1, a2, a3, a4, a5, a6, a7);
}

static bool PatchImport(HMODULE mod, const char* dll, const char* name, void* hook, void** orig) {
    uint8_t* base = (uint8_t*)mod;
    auto dos = (IMAGE_DOS_HEADER*)base;
    auto nt = (IMAGE_NT_HEADERS64*)(base + dos->e_lfanew);
    auto& dir = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT];
    for (auto imp = (IMAGE_IMPORT_DESCRIPTOR*)(base + dir.VirtualAddress); imp->Name; imp++) {
        if (_stricmp((const char*)(base + imp->Name), dll) != 0) continue;
        auto names = (IMAGE_THUNK_DATA64*)(base + (imp->OriginalFirstThunk ? imp->OriginalFirstThunk : imp->FirstThunk));
        auto funcs = (IMAGE_THUNK_DATA64*)(base + imp->FirstThunk);
        for (; names->u1.AddressOfData; names++, funcs++) {
            if (IMAGE_SNAP_BY_ORDINAL64(names->u1.Ordinal)) continue;
            auto ibn = (IMAGE_IMPORT_BY_NAME*)(base + names->u1.AddressOfData);
            if (strcmp((const char*)ibn->Name, name) != 0) continue;
            DWORD old;
            if (!VirtualProtect(&funcs->u1.Function, 8, PAGE_READWRITE, &old)) return false;
            *orig = (void*)funcs->u1.Function;
            funcs->u1.Function = (ULONGLONG)hook;
            VirtualProtect(&funcs->u1.Function, 8, old, &old);
            return true;
        }
    }
    return false;
}

static DWORD WINAPI HookPanorama(LPVOID) {
    HMODULE pano = nullptr;
    for (int i = 0; i < 3000 && !(pano = GetModuleHandleA("panorama.dll")); i++) Sleep(100);
    if (!pano) {
        Log("[-] panorama.dll not loaded, in-game pick button disabled");
        return 1;
    }
    uint8_t* target = FindRunScript(pano);
    if (!target || !InstallHook(target, kRunScriptStolen, (void*)&HookedRunScript, (void**)&g_OrigRunScript)) {
        Log("[-] CUIEngine::RunScript not found, in-game pick button disabled");
        return 1;
    }
    int n = PatchImport(pano, "tier0.dll", "Msg", (void*)&HookedMsg, (void**)&g_OrigMsg)
          + PatchImport(pano, "tier0.dll", "LoggingSystem_Log", (void*)&HookedLog, (void**)&g_OrigLog)
          + PatchImport(pano, "tier0.dll", "LoggingSystem_LogDirect", (void*)&HookedLogDirect, (void**)&g_OrigLogDirect);
    Log("[+] Hooked %d tier0 log imports of panorama.dll (script messages)", n);
    Log("[+] Hooked CUIEngine::RunScript at panorama.dll+0x%llX (pick button)",
        (unsigned long long)(target - (uint8_t*)pano));
    return 0;
}

static DWORD WINAPI MainThread(LPVOID) {
    HMODULE rs = nullptr;
    for (int i = 0; i < 600 && !(rs = GetModuleHandleA("resourcesystem.dll")); i++) Sleep(100);
    HMODULE tier0 = GetModuleHandleA("tier0.dll");
    if (!rs || !tier0) {
        Log("[-] resourcesystem.dll / tier0.dll not loaded");
        return 1;
    }

    g_Purge = (PurgeFn)GetProcAddress(tier0, "?Purge@CBufferString@@QEAAXH@Z");
    if (!g_Purge) {
        Log("[-] CBufferString::Purge not found in tier0.dll");
        return 1;
    }


    std::string cfg = g_appDir + "model_swap.cfg";
    if (!LoadConfig(cfg)) {
        Log("[*] No config at %s yet, waiting for the launcher", cfg.c_str());
    }
    CreateThread(nullptr, 0, WatchConfig, nullptr, 0, nullptr);

    uint8_t* target = FindPattern(rs, kInitForFileReadSig, sizeof(kInitForFileReadSig));
    if (!target) {
        Log("[-] InitForFileRead signature not found (game updated?)");
        return 1;
    }

    uint8_t* lea = target + kResourceSystemLeaOffset;
    if (lea[0] != 0x48 || lea[1] != 0x8D || lea[2] != 0x0D) {
        Log("[-] Unexpected code at InitForFileRead+0x%zX (game updated?)", kResourceSystemLeaOffset);
        return 1;
    }
    g_ResourceSystem = lea + 7 + *(int32_t*)(lea + 3);

    if (!InstallHook(target, kStolenBytes, (void*)&HookedInitForFileRead, (void**)&g_OrigInitForFileRead)) {
        Log("[-] Failed to install hook");
        return 1;
    }
    Log("[+] Hooked InitForFileRead at resourcesystem.dll+0x%llX",
        (unsigned long long)(target - (uint8_t*)rs));

    static const uint8_t kBlockingLoadSig[] = {
        0x40, 0x53, 0x55, 0x57, 0x48, 0x81, 0xEC, 0x80, 0x00, 0x00, 0x00, 0x48, 0x8B, 0x01, 0x49, 0x8B,
        0xE8, 0x48, 0x8B, 0xFA, 0x48, 0x8B, 0xD9, 0xFF, 0x90, 0x98, 0x01, 0x00, 0x00
    };
    static const uint8_t kSetNameSig[] = {
        0x48, 0x89, 0x5C, 0x24, 0x10, 0x57, 0x48, 0x83, 0xEC, 0x30, 0x8B, 0x41, 0x04, 0x48, 0x8D, 0x79,
        0x08, 0x48, 0x8B, 0xD9, 0xA9, 0xFF, 0xFF, 0xFF, 0x3F, 0x76, 0x0F, 0x0F, 0xBA, 0xE0, 0x1E
    };
    g_BlockingLoad = (BlockingLoadFn)FindPattern(rs, kBlockingLoadSig, sizeof(kBlockingLoadSig));
    g_SetResourceName = (SetResourceNameFn)FindPattern(rs, kSetNameSig, sizeof(kSetNameSig));
    Log("[%c] Menu preview refresh: BlockingLoad %s, SetResourceName %s",
        g_BlockingLoad && g_SetResourceName ? '+' : '-', g_BlockingLoad ? "found" : "missing", g_SetResourceName ? "found" : "missing");

    uint8_t* fr = FindPattern(rs, kFindOrRegisterSig, sizeof(kFindOrRegisterSig));
    if (fr && g_SetResourceName &&
        InstallHook(fr, kFindOrRegisterStolen, (void*)&HookedFindOrRegister, (void**)&g_OrigFindOrRegister))
        Log("[+] Hooked FindOrRegisterResourceByName at resourcesystem.dll+0x%llX (menu refresh)",
            (unsigned long long)(fr - (uint8_t*)rs));
    else
        Log("[-] FindOrRegisterResourceByName not hooked, menu preview refresh disabled");

    CreateThread(nullptr, 0, HookPanorama, nullptr, 0, nullptr);
    CreateThread(nullptr, 0, WriteEquips, nullptr, 0, nullptr);
    CreateThread(nullptr, 0, ResolveItemHosts, nullptr, 0, nullptr);
    return 0;
}



BOOL APIENTRY DllMain(HMODULE hModule, DWORD reason, LPVOID) {
    if (reason == DLL_PROCESS_ATTACH) {
        DisableThreadLibraryCalls(hModule);
        InitializeCriticalSection(&g_logLock);

        const char* appdata = getenv("APPDATA");
        g_appDir = std::string(appdata ? appdata : ".") + "\\dota-skins\\";
        // the last run's log is kept (a crash or a match is often looked at after a restart)
        MoveFileExA((g_appDir + "model_swap.log").c_str(), (g_appDir + "model_swap.prev.log").c_str(), MOVEFILE_REPLACE_EXISTING);
        g_log = fopen((g_appDir + "model_swap.log").c_str(), "w");

        Log("[*] changer.dll attached (PID=%lu) t=%llu", GetCurrentProcessId(), GetTickCount64());
        CreateThread(nullptr, 0, MainThread, nullptr, 0, nullptr);
    }
    return TRUE;
}
