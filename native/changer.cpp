// changer.dll — подмена файлов ресурсов Dota 2 в памяти (режим «в памяти» лаунчера).
//
// Хук CAsyncResourceDataRequest::InitForFileRead (resourcesystem.dll): имя ресурса
// не меняется (он остаётся в манифесте предзагрузки и грузится штатно), подменяется
// только файл, из которого читаются его данные (crystal_maiden_cape.vmdl_c ->
// lumini_polare_back.vmdl_c).
//
// Таблицу подмен пишет лаунчер: %APPDATA%\dota-skins\model_swap.cfg
// (перечитывается при изменении, действует на ресурсы, загруженные после этого)
//   <оригинал>|<замена>     пути без суффикса _c
//   hide|<папка>/           все .vmdl из папки -> невидимая модель
//   <путь>|keep             исключение из hide
// Лог: %APPDATA%\dota-skins\model_swap.log
//
// Сборка (MinGW): g++ -O2 -shared -static -o changer.dll changer.cpp -lpsapi

#include <windows.h>
#include <psapi.h>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <cstdarg>
#include <string>
#include <vector>
#include <set>
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
// +0: uint32 length (биты 0..29), +4: uint32 allocated (биты 0..29) | 0x40000000 = данные inline на +8
// +8: char* или inline-буфер
static const char* BufStr_Get(void* s) {
    uint32_t alloc = *(uint32_t*)((char*)s + 4);
    if (alloc & 0x40000000) return (const char*)s + 8;
    if ((alloc & 0x3FFFFFFF) == 0) return "";
    return *(const char**)((char*)s + 8);
}

using PurgeFn = void (*)(void* self, int allocatedSize);
static PurgeFn g_Purge = nullptr;

// CBufferString со встроенным буфером на 200 байт, как его создаёт сам движок
struct StackBufferString {
    uint32_t length = 0;
    uint32_t alloc = 0xC00000C8;
    char data[200 + 8] = {};
    ~StackBufferString() { g_Purge(this, 0); }
};

// --- Таблица подмен ---
// Лаунчер перезаписывает конфиг на ходу: таблица собирается заново и подменяется
// целиком под эксклюзивной блокировкой, хук читает её под разделяемой.
struct Rules {
    std::unordered_map<std::string, std::string> replacements;
    std::vector<std::string> hiddenPrefixes;
    std::set<std::string> keep;
};
static Rules* g_rules = new Rules();
static std::string g_lang = "ru";  // язык кнопок интерфейса
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
        rules.replacements.emplace(f, t);  // первая подмена файла побеждает, как в файловом режиме
    }
}

static void QueuePreloads(const Rules& rules);
// Свечения предметов для матча (строки glow|npc|particle|cp:type:attachment;...):
// JS-массив, который скрипт интерфейса получает через DSLSetGlows
static std::string g_glowsJs = "[]";
// Предметы для пустых слотов, которые запущенная Dota ещё не прочитала (строка
// restart|имя\tимя): их кнопка пишет «после перезапуска»
static std::string g_restartJs = "[]";
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

// glow|npc|particle|cp:type:attachment;... -> {npc:"",fx:"",cps:[[cp,"type","att"],...]}
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
    return "{npc:" + JsString(f[1]) + ",fx:" + JsString(f[2]) + ",cps:[" + cps + "]}";
}
static volatile LONG g_refreshPending = 0;  // конфиг сменился: обновить предпросмотр после предзагрузки

static bool LoadConfig(const std::string& path) {
    std::ifstream file(path);
    if (!file.is_open()) return false;
    Rules* fresh = new Rules();
    std::string line, glows, restart;
    std::string lang = "ru";
    while (std::getline(file, line)) {
        if (!line.empty() && line.back() == '\r') line.pop_back();
        if (line.empty() || line[0] == '#') continue;
        if (line.compare(0, 5, "lang|") == 0) { lang = line.substr(5); continue; }
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
            continue;
        }
        size_t pos = line.find('|');
        if (pos != std::string::npos) AddRule(*fresh, line.substr(0, pos), line.substr(pos + 1));
    }
    AcquireSRWLockExclusive(&g_rulesLock);
    g_lang = (lang == "en") ? "en" : "ru";
    Rules* old = g_rules;
    g_rules = fresh;
    ReleaseSRWLockExclusive(&g_rulesLock);
    AcquireSRWLockExclusive(&g_glowsLock);
    g_glowsJs = "[" + glows + "]";
    g_restartJs = "[" + restart + "]";
    ReleaseSRWLockExclusive(&g_glowsLock);
    InterlockedIncrement(&g_glowsVersion);
    static bool first = true;
    if (!first) InterlockedExchange(&g_refreshPending, 1);
    first = false;
    delete old;
    QueuePreloads(*fresh);
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

// Лаунчер переписывает конфиг при смене пресета — перечитываем его
static DWORD WINAPI WatchConfig(LPVOID) {
    std::string cfg = g_appDir + "model_swap.cfg";
    FILETIME last{};
    FileTime(cfg, last);
    for (;;) {
        Sleep(50);
        FILETIME now;
        if (FileTime(cfg, now) && CompareFileTime(&now, &last) != 0) {
            last = now;
            Sleep(10);  // лаунчер пишет через временный файл и переименование
            LoadConfig(cfg);
        }
    }
}

// --- Хук InitForFileRead ---
// void InitForFileRead(this, const ResourceBindingBase_t* binding, priority, bool, const char* fileName)
// fileName обычно nullptr — тогда движок сам получает имя через ResourceSystem->vtable[0x180/8]
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

// --- Предзагрузка замен для меню ---
// Имя ресурса движка: CBufferString с буфером на 200 байт, затем хэш и тип.
static volatile LONG g_inMenu = 0;  // скрипт интерфейса сообщает: меню, не матч
static volatile LONG g_substituted = 0;     // сколько раз отдали замену вместо оригинала

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

static std::set<std::string> g_preloadQueue;  // замены, которые ещё надо загрузить
static std::set<std::string> g_resident;      // замены, уже загруженные: их имя можно подставлять
static SRWLOCK g_preloadLock = SRWLOCK_INIT;

// Ресурсы, которые в меню отдаём заменой по имени (обход кэша): модели и эффекты
static bool IsSwappableResource(const char* name) {
    size_t len = strlen(name);
    return len > 5 && (_stricmp(name + len - 5, ".vmdl") == 0 || _stricmp(name + len - 5, ".vpcf") == 0 ||
                       _stricmp(name + len - 5, ".vmat") == 0);
}

static void QueuePreloads(const Rules& rules) {
    AcquireSRWLockExclusive(&g_preloadLock);
    for (auto& r : rules.replacements) {
        const std::string& t = r.second;
        if (IsSwappableResource(t.c_str()) && !g_resident.count(t)) g_preloadQueue.insert(t);
    }
    ReleaseSRWLockExclusive(&g_preloadLock);
}

static bool PreloadsDone() {
    AcquireSRWLockShared(&g_preloadLock);
    bool r = g_preloadQueue.empty();
    ReleaseSRWLockShared(&g_preloadLock);
    return r;
}

// Модели и материалы загружены (эффекты ещё могут грузиться): модель героя уже
// можно перестроить, эффекты появятся вторым обновлением
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

// Поток интерфейса (тик скрипта): синхронно грузим по несколько замен за тик
static void RunPreloads() {
    if (!g_SetResourceName || !g_BlockingLoad || !g_ResourceSystem) return;
    // сколько успеем за ~30 мс тика (тик каждые 50 мс): сет грузится за доли секунды
    ULONGLONG start = GetTickCount64();
    while (GetTickCount64() - start < 30) {
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


// --- Подмена при поиске ресурса (меню) ---
// CResourceSystem::FindOrRegisterResourceByName_Internal(rs, ResourceName*, bool):
// по имени и хэшу отдаёт привязку ресурса. В меню для оригинала, у которого
// загруженная замена, отдаём привязку замены — как при смене предмета кнопкой
// «Надеть», панель получает другой ресурс и перерисовывает модель.
using FindOrRegisterFn = void* (*)(void* rs, ResourceName* name, uintptr_t flag);
static FindOrRegisterFn g_OrigFindOrRegister = nullptr;

static void* HookedFindOrRegister(void* rs, ResourceName* name, uintptr_t flag) {
    // и в меню, и в матче: ресурс, загруженный до выбора (кольцо эмблемы, шейдер
    // экрана), иначе остаётся в кэше со старой подменой
    if (name && g_SetResourceName) {
        const char* cur = BufStr_Get(name);
        size_t len = strlen(cur);
        std::string repl;
        if (len > 5 && IsSwappableResource(cur) && FindReplacementFile(cur, repl) && IsResident(repl)) {
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
// Первые 17 байт (до sub rsp) не содержат RIP-relative инструкций — переносим их в трамплин
static const size_t kStolenBytes = 17;
// По смещению +0x58: lea rcx,[rip+disp32] — адрес глобального объекта ResourceSystem
static const size_t kResourceSystemLeaOffset = 0x58;

static uint8_t* FindPattern(HMODULE mod, const uint8_t* sig, size_t sigLen) {
    MODULEINFO mi{};
    if (!GetModuleInformation(GetCurrentProcess(), mod, &mi, sizeof(mi))) return nullptr;
    uint8_t* base = (uint8_t*)mi.lpBaseOfDll;
    uint8_t* found = nullptr;
    for (size_t i = 0; i + sigLen <= mi.SizeOfImage; i++) {
        if (memcmp(base + i, sig, sigLen) == 0) {
            if (found) return nullptr;  // сигнатура неуникальна — не рискуем
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

// stolen — длина целых инструкций в начале функции (>= 14, без RIP-relative)
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

// --- Нажатия «ВЫБРАТЬ» ---
// Записываются в equips.txt (`tick|class|from|def|labels`), лаунчер надевает предмет
// по номеру или названию из строки.
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

// --- Кнопка «Выбрать» в окне предмета Dota (Panorama) ---
// Перехватываем CUIEngine::RunScript (panorama.dll): из потока интерфейса, внутри
// обычного вызова игры, раз в секунду выполняем свой JS. Он находит открытые окна
// предмета (DOTAStoreItemDetailsPage) и добавляет кнопку «Выбрать». Нажатие
// выполняет через RunScriptInPanelContext скрипт-метку /*DSL_PICK*/, которую видит
// этот же хук и пишет в equips.txt: лаунчер надевает предмет из строки.
using RunScriptFn = void (*)(void* engine, void* panel, const char* script, const char* path, uint64_t lineCol);
static RunScriptFn g_OrigRunScript = nullptr;
static thread_local bool t_inOurScript = false;
static ULONGLONG g_lastInject = 0;

static const char kPickJs[] = R"JS(
if (typeof DSLTxt === "undefined") DSLTxt = { select: "SELECT", selected: "SELECTED", have: "OWNED", restart: "AFTER RESTART" };
(function () {
  var ctx = $.GetContextPanel();
  if (!ctx) return;
  var root = ctx;
  while (root.GetParent()) root = root.GetParent();
  var me = "" + Math.random();
  // живой цикл (сердцебиение свежее 2 с) не заменяем
  var hb = parseFloat(root.GetAttributeString("dsl_hb", "0")) || 0;
  if (Date.now() - hb < 2000) return;
  root.SetAttributeString("dsl_loop", me);
  var last = "";
  var lastMenu = null;
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
        // любая открытая страница меню с карточками предметов: страница героя,
        // «Окружение» (варды, крипы, башни, эмблемы…), арсенал
        if (!hp.visible || hp.paneltype === "DOTAStoreItemDetailsPage") continue;
        // карточки предметов: у каждой есть ItemInfoContainer (у недоступных нет кнопки покупки)
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
          // свой предмет: класс Owned; текст статуса («ЕСТЬ У ВАС») — только если карточка не «НЕДОСТУПНО»
          var owned = DSLOwned(box) || (!!statusText && !unavailable);
          var wrapHave = box.FindChild("DSLPickWrap");
          if (wrapHave) wrapHave.visible = !owned;
          if (nativeStatus && nativeStatus.GetAttributeString("dsl_hidden", "") === "1" && owned) {
            nativeStatus.visible = true;
            nativeStatus.SetAttributeString("dsl_hidden", "");
          }
          if (owned) continue;
          // блок покупки с ценой прячем; у недоступных — родную надпись «НЕДОСТУПНО» и затемнение
          var multi = box.FindChild("AvailableItemPurchaseButton");
          if (multi) multi.visible = false;
          if (unavailable && nativeStatus) { nativeStatus.visible = false; nativeStatus.SetAttributeString("dsl_hidden", "1"); }
          card.style.opacity = "1";
          card.style.brightness = "1";
          var chain = [row.paneltype + "#" + row.id];
          var have = box.FindChildTraverse("DSLPickButton");
          if (have) { DSLSetPicked(have, have.GetChild(0), !!picked[DSLKey(row)]); continue; }
          // как у карточки Dota: внизу строка статуса; при наведении на карточку
          // она гаснет и на её месте появляется кнопка «ВЫБРАТЬ» во всю ширину
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
              // остальные строки этого списка: снова «ВЫБРАТЬ» (в слоте один предмет)
              // строки вложены в обёртки: поднимаемся до контейнера со всем списком
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
      // переключатель персоны (КЛАССИКА / ВЭЙ): у неимеющейся персоны кнопка Dota
      // выключена — включаем и шлём лаунчеру «переключить персону героя»
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
      // свечения предметов: в матче на своём герое, в меню на моделях героев
      $.Msg("DSL_GTICK");
      DSLGlowTick(inMenu);
      var log = types.filter(function (t) { return t.indexOf("DOTA") !== 0; }).join(", ");
      if (log && log !== last) { last = log; $.Msg("DSL_LOG " + log); }
    } catch (e) {
      $.Msg("DSL_LOG error " + e);
    }
    $.Schedule(0.5, tick);
  }
  // номер предмета (item def) из строки: свойство элемента DOTAEconItem
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
  // тексты строки: среди них название предмета или сета
  // тексты строки (без нашей кнопки): среди них название предмета или сета
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
  // все наши кнопки внутри панели
  DSLButtons = function (root) {
    var out = [], stack = [root];
    while (stack.length) {
      var p = stack.pop();
      if (p.id === "DSLPickButton") { out.push(p); continue; }
      for (var c = 0; c < p.GetChildCount(); c++) stack.push(p.GetChild(c));
    }
    return out;
  };
  // строка предмета кнопки: предок, родитель которого содержит ItemInfoContainer
  DSLRowOf = function (btn) {
    var row = btn.GetParent();
    for (var u = 0; u < 6 && row; u++) {
      if (row.GetParent() && row.GetParent().FindChild("ItemInfoContainer") === row) return row.GetParent();
      row = row.GetParent();
    }
    return null;
  };
  var picked = {};  // ключ строки -> выбрано
  // «ВЫБРАТЬ» — тёмно-серая кнопка, как у Dota; «ВЫБРАНО» — надпись без фона
  // как в карточке Dota (ui_econ_item_animated): «ВЫБРАТЬ» — кнопка ButtonPrimary DarkGrey
  // во всю ширину внизу карточки; «ВЫБРАНО» — строка статуса (белый текст со свечением)
  // «ВЫБРАНО» — в строке статуса (белый текст со свечением), кнопки при наведении нет
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
  // предметы для пустых слотов, которые появятся после перезапуска Dota: у выбранной
  // карточки вместо «ВЫБРАНО» — «ПОСЛЕ ПЕРЕЗАПУСКА». DLL зовёт это изнутри своего
  // вызова: здесь только запоминаем, надписи меняет тик
  var restartNames = {}, restartVer = 0, pendingRestart = null;
  DSLSetRestart = function (list) { pendingRestart = list || []; };
  function doneText(it) {
    var labels = DSLLabels(it.row);
    for (var i = 0; i < labels.length; i++) {
      if (restartNames[("" + labels[i]).trim().toLowerCase()]) return true;
    }
    return false;
  }
  // наведение на карточку (как .Container:hover у Dota): кнопка вместо строки статуса
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
    // частый тик для DLL (в меню): подгрузка замен и обновление модели без ожидания
    if (lastMenu && root.IsValid() && root.GetAttributeString("dsl_loop", "") === me) $.Msg("DSL_TICK");
    $.Schedule(0.05, hoverTick);
  }
  hoverTick();
  // класс Owned ставится на карточку (DOTAEconItemAnimated или её Container): ищем у предков
  DSLOwned = function (p) {
    for (var u = 0; p && u < 8; u++, p = p.GetParent()) {
      if (p.BHasClass("Owned")) return true;
      if (p.paneltype === "DOTAEconItemAnimated") break;
    }
    return false;
  };
  // --- Свечения предметов в матче ---
  // Эффекты, которые предмет создаёт сам (particle_create), а файлами их не подменить:
  // создаём на своём герое с точками привязки из файла эффекта. Список даёт DLL.
  var glowList = [], glowFx = {};  // ключ "сущность|эффект" -> индекс эффекта
  // Эффект убираем через API, только пока его сущность жива: при загрузке карты
  // («Опробовать») модели меню удаляются, и вызов на мёртвой сущности роняет игру.
  // Эффекты удалённой сущности игра убирает сама — их просто забываем.
  function dropGlow(k) {
    var ent = parseInt(k, 10);
    if (Entities.IsValidEntity(ent)) {
      try { Particles.DestroyParticleEffect(glowFx[k], false); Particles.ReleaseParticleIndex(glowFx[k]); } catch (e6) {}
    }
    delete glowFx[k];
  }
  DSLClearGlows = function () {
    for (var k in glowFx) dropGlow(k);
    glowFx = {};
  };
  // DLL вызывает это изнутри другого вызова JS: здесь только запоминаем список,
  // эффекты меняет обычный тик (иначе игра падает, если модель как раз перестраивается)
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
      $.Msg("DSL_LOG glows: " + glowList.length);
      return;  // новые эффекты — на следующем тике
    }
    var alive = {};
    if (glowList.length) {
      var wanted = {};
      for (var g0 = 0; g0 < glowList.length; g0++) wanted[glowList[g0].npc] = true;
      var targets = [];
      if (inMenu) {
        // модели героев на страницах меню: клиентские сущности с именем героя
        var ents = Entities.GetAllEntities();
        for (var i = 0; i < ents.length; i++) {
          if (!Entities.IsValidEntity(ents[i])) continue;
          var nm = "";
          try { nm = Entities.GetUnitName(ents[i]); } catch (e8) {}
          if (nm && wanted[nm]) targets.push([ents[i], nm]);
        }
      } else {
        // в матче только свой герой: на чужих эффект мог бы выдать невидимого
        var hero = Players.GetPlayerHeroEntityIndex(Players.GetLocalPlayer());
        if (hero > 0 && Entities.IsValidEntity(hero)) targets.push([hero, Entities.GetUnitName(hero)]);
      }
      for (var t = 0; t < targets.length; t++) {
        var ent = targets[t][0], name = targets[t][1];
        for (var g = 0; g < glowList.length; g++) {
          var gl = glowList[g];
          if (gl.npc !== name) continue;
          var key = ent + "|" + gl.fx;
          alive[key] = true;
          if (key in glowFx) continue;
          var p = Particles.CreateParticle(gl.fx, ParticleAttachment_t.PATTACH_CUSTOMORIGIN, ent);
          for (var c = 0; c < gl.cps.length; c++) {
            var cp = gl.cps[c];
            var kind = ParticleAttachment_t[cp[1]];
            if (kind === undefined) kind = ParticleAttachment_t.PATTACH_ABSORIGIN_FOLLOW;
            Particles.SetParticleControlEnt(p, cp[0], ent, kind, cp[2], [0, 0, 0], true);
          }
          glowFx[key] = p;
        }
      }
    }
    for (var k in glowFx) {
      if (!alive[k]) dropGlow(k);
    }
  };
  $.Msg("DSL_LOOP");
  $.Msg("DSL_LOG loop started in " + ctx.id);
  // вызывается из DLL, когда замены загружены: перестроить модель героя
  DSLRefresh = function () {
    var pages = root.FindChildrenWithClassTraverse("DashboardPage");
    for (var i = 0; i < pages.length; i++) {
      var hp = pages[i];
      if (hp.paneltype !== "DOTAHeroPageNewV2" || !hp.visible) continue;
      // то же событие, что Dota шлёт после «Надеть»: модель перестраивается без перезагрузки страницы
      // примерка держит свою модель поверх набора: сначала снимаем её («ОТМЕНА»,
      // DOTASetPreviewRevert). Снимаем и когда кнопка скрыта: после выхода из
      // гардероба примерка остаётся, а панель с кнопкой не видна
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
      $.Schedule(tryOn ? 0.15 : 0, function () { $.DispatchEvent("DOTALoadoutChanged"); });
      // «ОТМЕНА» перестраивает модель асинхронно и может перекрыть первое обновление: повторяем;
      // после примерки сета Dota возвращает прежнюю модель из кэша позже — ещё раз через 2 с
      $.Schedule(0.8, function () { $.DispatchEvent("DOTALoadoutChanged"); });
      if (tryOn) $.Schedule(2.0, function () { $.DispatchEvent("DOTALoadoutChanged"); });
      $.Msg("DSL_LOG refresh: loadout changed (revert " + (revert ? (shown ? "shown" : "hidden") : "none") + ", controls " + (controls ? (controls.visible ? "shown" : "hidden") : "none") + ")");
      return;
    }
  };
  tick();
})();
)JS";

// payload: "<def>|<label>\t<label>..." — номер предмета и подписи строки
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
static void* g_injectPanel = nullptr;  // панель текущего внедрения скрипта
static void* g_loopPanel = nullptr;    // контекст живого цикла скрипта

// Поток интерфейса, из тика скрипта: замены загружены — просим скрипт перерисовать модель
// Поток интерфейса, из тика скрипта в матче: новая таблица свечений -> DSLSetGlows
static LONG g_glowsSent = -1;
static void MaybeSendGlows() {
    LONG v = g_glowsVersion;
    if (v == g_glowsSent || !g_uiEngine || !g_loopPanel || !g_OrigRunScript) return;
    g_glowsSent = v;
    AcquireSRWLockShared(&g_glowsLock);
    std::string script = "if (typeof DSLSetGlows === 'function') DSLSetGlows(" + g_glowsJs + ");"
                         "if (typeof DSLSetRestart === 'function') DSLSetRestart(" + g_restartJs + ");";
    ReleaseSRWLockShared(&g_glowsLock);
    Log("[*] Sending item glows to the game UI");
    t_inOurScript = true;
    g_OrigRunScript(g_uiEngine, g_loopPanel, script.c_str(), "dota_skins_glows.js", 0);
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
    // 1 — конфиг сменился; 2 — модель уже перестроена, ждём эффекты
    static bool modelsRefreshed = false;
    if (!g_refreshPending || !g_uiEngine || !g_loopPanel || !g_OrigRunScript) return;
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
    t_inOurScript = true;
    g_OrigRunScript(g_uiEngine, g_loopPanel, "if (typeof DSLRefresh === 'function') DSLRefresh();", "dota_skins_refresh.js", 0);
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
    if (now - g_lastInject < 5000) return;
    g_lastInject = now;
    t_inOurScript = true;
    g_uiEngine = engine;
    g_injectPanel = panel;
    // тексты кнопок на выбранном языке — перед основным скриптом
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
static const size_t kRunScriptStolen = sizeof(kRunScriptProlog);  // 19 байт, без RIP-relative

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
    // lea rcx,[rip+disp32] -> строка; затем назад до пролога (функция ~0x70 байт до lea)
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

// $.Msg из скрипта уходит в tier0 (Msg / LoggingSystem_Log*): перехватываем импорт
// panorama.dll и ловим строки DSL_PICK / DSL_LOG.
static bool CheckDslMessage(const char* msg) {
    if (!msg) return false;
    const char* p = strstr(msg, "DSL_");
    if (!p) {
        if (strstr(msg, "SetPreview")) Log("[ui] %.600s", msg);
        return false;
    }
    if (strncmp(p, "DSL_TICK", 8) == 0) { if (g_inMenu) { RunPreloads(); MaybeRefreshPreview(); } }
    else if (strncmp(p, "DSL_GTICK", 9) == 0) MaybeSendGlows();
    else if (strncmp(p, "DSL_LOOP", 8) == 0) { if (g_injectPanel) { g_loopPanel = g_injectPanel; g_glowsSent = -1; } }
    else if (strncmp(p, "DSL_PICK", 8) == 0) RecordPick(p[8] == ' ' ? p + 9 : nullptr);
    else if (strncmp(p, "DSL_MENU", 8) == 0) {
        LONG v = p[9] == '1';
        if (InterlockedExchange(&g_inMenu, v) != v) Log("[*] %s", v ? "In menu: renaming swapped models" : "In game: file swap only");
    }
    else if (strncmp(p, "DSL_LOG", 7) == 0) Log("[js] %.3800s", p + 7);
    else return false;
    return true;
}

using Var8Fn = uint64_t (*)(uint64_t, uint64_t, uint64_t, uint64_t, uint64_t, uint64_t, uint64_t, uint64_t);
static Var8Fn g_OrigMsg = nullptr, g_OrigLog = nullptr, g_OrigLogDirect = nullptr;

// Сообщение по формату и аргументам (до 7 целых/указателей, как пришли в регистрах/стеке)
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
    return 0;
}



BOOL APIENTRY DllMain(HMODULE hModule, DWORD reason, LPVOID) {
    if (reason == DLL_PROCESS_ATTACH) {
        DisableThreadLibraryCalls(hModule);
        InitializeCriticalSection(&g_logLock);

        const char* appdata = getenv("APPDATA");
        g_appDir = std::string(appdata ? appdata : ".") + "\\dota-skins\\";
        g_log = fopen((g_appDir + "model_swap.log").c_str(), "w");

        Log("[*] changer.dll attached (PID=%lu) t=%llu", GetCurrentProcessId(), GetTickCount64());
        CreateThread(nullptr, 0, MainThread, nullptr, 0, nullptr);
    }
    return TRUE;
}
