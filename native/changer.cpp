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
#include <cfloat>
#include <string>
#include <vector>
#include <set>
#include <map>
#include <fstream>
#include <unordered_map>
#include <algorithm>
#include <cctype>
#include <malloc.h>

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
    // a hero item's swap -> its heroes (npc names): in a match it applies to the hero you
    // play only, the other heroes keep their own look (general swaps have none)
    std::unordered_map<std::string, std::vector<std::string>> heroes;
    std::vector<std::string> hiddenPrefixes;
    std::set<std::string> keep;
    // A file's swap in a match where the menu's differs (lines ...|match): a persona's, whose
    // look the menu shows as worn (see g_personaDefs) while its hero's classic items stay for
    // the menu's classic look (lines ...|menu: not in a match). See RuleFor.
    std::unordered_map<std::string, std::string> matchReplacements;
    std::unordered_map<std::string, std::vector<std::string>> matchHeroes;  // a persona swap's heroes
    std::set<std::string> menuOnly;
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

static std::vector<std::string> Split(const std::string& s, char sep);

enum RuleMode { kRuleAlways, kRuleMatch, kRuleMenu };

static void AddRule(Rules& rules, const std::string& from, const std::string& to, const std::string& heroes, RuleMode mode) {
    std::string f = Normalize(from), t = Normalize(to);
    if (f.empty() || t.empty() || f == t) return;
    if (f == "hide") {
        if (t.back() != '/') t += '/';
        rules.hiddenPrefixes.push_back(t);
    } else if (t == "keep") {
        rules.keep.insert(f);
    } else if (mode == kRuleMatch) {
        if (rules.matchReplacements.emplace(f, t).second) {
            for (auto& h : Split(heroes, ','))
                if (!h.empty()) rules.matchHeroes[f].push_back(Normalize(h));
            if (!rules.heroes.count(f)) rules.heroes[f] = rules.matchHeroes[f];
        }
    } else if (rules.replacements.emplace(f, t).second) {  // the first swap of a file wins, as in the file mode
        auto& list = rules.heroes[f];
        list.clear();
        for (auto& h : Split(heroes, ','))
            if (!h.empty()) list.push_back(Normalize(h));
        if (mode == kRuleMenu) rules.menuOnly.insert(f);
    }
}

// The hero you play in this match (npc name), given by the interface script once picked;
// empty in the menu and before the pick. Read and written under g_rulesLock.
static std::string g_localNpc;
// the interface script reports: menu, not a match. Dota starts in the menu: before the
// script's first report (the dashboard's loading screen) the menu's rules hold, or a hero's
// items loaded then (the front page's heroes) kept their own files for the whole session
static volatile LONG g_inMenu = 1;
// A match before its game starts (hero pick, strategy, showcase: DSL_PREGAME from the script):
// a persona's models are put on the strategy screen's preview live (its switch may take them
// off again); after it the entities made get them by name (see HookedFindOrRegister)
static volatile LONG g_preGame = 1;

// The game's interface state (ChangeGameUIState): 0 none, 1 the loading screen, 2 a game,
// 3 the dashboard; -1 not seen yet. The interface script's "menu" (the game state 0) lasts
// through a match's loading screen, where the match's own resources are registered.
static volatile LONG g_uiState = -1;
static const LONG kUiLoading = 1, kUiInGame = 2;

// The real menu: the script's menu with no match loading or running behind it (unknown
// state: the script's word alone)
static bool MenuShown() {
    LONG s = g_uiState;
    return g_inMenu && s != kUiLoading && s != kUiInGame;
}

// Whether a swap applies now: every swap in the menu; in a match a hero item's swap only
// for the hero you play (an enemy or ally on another hero keeps their own items), the
// general ones (couriers, wards, terrain...) always. Under g_rulesLock.
static bool RuleActive(const Rules& rules, const std::string& dst) {
    if (g_inMenu) return true;
    auto h = rules.heroes.find(dst);
    if (h == rules.heroes.end() || h->second.empty()) return true;
    for (auto& npc : h->second)
        if (npc == g_localNpc) return true;
    return false;
}

// The heroes (npc names) with a persona, and those shown in it now (the preset's word, or a
// switch just pressed on the hero page or the strategy screen). Under g_rulesLock.
static std::set<std::string> g_personaNpcs, g_personaShownNpcs;

// Whether a file's persona swap (persona true, lines ...|match) or its classic look's swap
// (false, lines ...|menu) applies in a match: the one of the look its hero is shown in now
// (the switch, before the launcher has put it into the preset). A hero with no persona
// known: as the table says (its match swaps, not the menu's). Under g_rulesLock.
static bool PersonaLookOn(const Rules& rules, const std::string& dst, bool persona) {
    // the persona swap's own heroes first (the classic look's swap of the same file may name
    // none: Legion Commander's weapon), then the other's
    for (auto* map : {&rules.matchHeroes, &rules.heroes}) {
        auto h = map->find(dst);
        if (h == map->end()) continue;
        for (auto& npc : h->second)
            if (g_personaNpcs.count(npc)) return (g_personaShownNpcs.count(npc) > 0) == persona;
    }
    return persona;
}

// The replacement of a file that applies now, nullptr for none: in a match its persona swap
// first, then its swap unless that one is the classic look's while the persona is shown.
// `forRead`: for reading the file itself, where a persona's models are never swapped: a match
// reads a model once (at the pick), and the strategy screen's switch may take the persona off
// again; its models are put on the hero live instead (LiveModelTick). Under g_rulesLock.
static const std::string* RuleFor(const Rules& rules, const std::string& dst, bool forRead = false) {
    const std::string* to = nullptr;
    if (!g_inMenu) {
        auto m = rules.matchReplacements.find(dst);
        bool model = dst.size() > 5 && dst.compare(dst.size() - 5, 5, ".vmdl") == 0;
        if (m != rules.matchReplacements.end() && !(forRead && model) && PersonaLookOn(rules, dst, true)) to = &m->second;
    }
    if (!to) {
        auto it = rules.replacements.find(dst);
        if (it == rules.replacements.end()) return nullptr;
        if (!g_inMenu && rules.menuOnly.count(dst) && !PersonaLookOn(rules, dst, false)) return nullptr;
        to = &it->second;
    }
    return RuleActive(rules, dst) ? to : nullptr;
}

// Whether `to` (RuleFor's answer for dst) is a persona's swap
static bool IsPersonaRule(const Rules& rules, const std::string& dst, const std::string* to) {
    auto m = rules.matchReplacements.find(dst);
    return to && m != rules.matchReplacements.end() && &m->second == to;
}

static void QueuePreloads(const Rules& rules, const std::set<std::string>& extra);
static bool IsSwappableResource(const char* name);
// Item glows for a match (lines glow|npc|particle|cp:type:attachment;...):
// a JS array the interface script gets through DSLSetGlows
static std::string g_glowsJs = "[]";
// Item models the glows hang on (the 5th field of glow lines), with every model the
// swap table turns into one of them: model in the game -> the item model
static std::unordered_map<std::string, std::string> g_itemModels;
// The glows as the DLL creates them itself in a match (the interface script cannot: a
// regular match gives its scripts no entities, so their effects hung on nothing)
// parent: on the hero the effect's model hangs on (its file's driver on "parent", "^" in the line)
struct GlowCp { int cp; int attach; std::string att; float vec[3]; bool world; bool parent = false; };
// place: where the item spawns it (its "spawn_in_loadout_only"): 0 everywhere, 1 on the
// menu's models and the strategy screen's (loadout) only, 2 in the game only
struct GlowDef { std::string npc, fx, item; std::vector<GlowCp> cps; std::vector<std::string> parts; bool hasParts; int place = 0; };
static std::vector<GlowDef> g_glowDefs;
// The emblem's attack effect (line attack|particle): added to the attacks of the hero
// you play, as the game adds an owned emblem's (see HookedCreateTracking)
static std::string g_attackFx;
// The preset's weather (line weather|type, 0 for none): set in the running game (see
// WeatherTick); -1 leaves cl_weather alone
static volatile LONG g_weatherWanted = -1;
// The preset's terrain (line terrain|map|override, map base names: dota|dota_desert) and
// HUD skin (line hudskin|skin): see HookedMapOverride and HookedSetHudSkin
static std::string g_terrainFrom, g_terrainTo, g_hudSkin;
// The preset's map effect (lines shader|map|material, shader|portrait|..., shader|post|...):
// see HookedSetShader
enum { kShaderMap, kShaderPortrait, kShaderPost, kShaderKinds };
static std::string g_shaders[kShaderKinds];
// The chosen styles' material groups (lines matgroup|model|group): see MatGroupTick
static std::unordered_map<std::string, std::string> g_matGroups;
// ... and mesh masks (lines meshmask|model|clear|set|reset): an arcana's style is its level,
// its hero's pieces' body group "arcana" (another mesh): the mask's bits of that group
// (clear), the level's (set), choice 0's (reset, when the item leaves). See MatGroupTick
struct MeshMask { uint64_t clear, set, reset; };
static std::unordered_map<std::string, MeshMask> g_meshMasks;
// Prismatic gems: the hero models' colours (lines gem|model|RRGGBB, as the game's Color:
// r, g, b, a bytes), see GemTick; for the item page's menu the palette (gemcolor|name|RRGGBB)
// and the colour picked per item definition (gempick|def|name), sent to the script
static std::unordered_map<std::string, uint32_t> g_gems;
static std::string g_gemColorsJs = "[]";
static std::string g_gemPicksJs = "{}";
static std::string g_gemItemsJs = "[]";
static std::string g_presetJs = "[]";  // picked|name: the preset's items, for the cards' «SELECTED»  // gemitem|name|icon: the items whose page offers a gem
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
// Personas (lines personadef|HERO|hero id|persona item|npc, persona|hero id): the hero page's
// persona switch for a persona the account does not own. The game asks its inventory for a
// hero's item in each loadout slot (GetItemInLoadout); for the persona slot of a hero whose
// persona the preset has it gets an item of that persona, made as the game makes the items
// it previews, and shows the persona as if worn: the switch, the model, the persona's slots
// and its wardrobe. In the menu, and in a match on the strategy screen for the hero you play
// (its switch and preview; the match's hero itself is the server's: the persona's models are
// put on it live, see LiveModelTick). See PersonaTick.
struct PersonaDef {
    int hero = 0, def = 0;
    std::string npc;
    bool operator==(const PersonaDef& o) const { return hero == o.hero && def == o.def && npc == o.npc; }
};
struct PersonaPick { bool on; ULONGLONG at; };
static std::map<std::string, PersonaDef> g_personaDefs;  // hero name (slot key) -> hero, persona item
static std::set<int> g_personaOn;                        // heroes whose persona the preset has
static std::map<int, PersonaPick> g_personaPicked;       // switched on the page, the preset not yet told
static SRWLOCK g_personaLock = SRWLOCK_INIT;
static volatile LONG g_personaVersion = 0;
static std::string g_personaKeysJs = "{}";  // the heroes shown in their persona, for the script
// The items the game shows as worn in the menu (lines wear|hero id|npc|slot|item|style): the
// preset's items of a hero with a body item no file can give (an arcana's skin, stance and
// effects: Legion Commander's Blades of Voth Domosh), the whole hero dressed by the game
struct WearItem {
    int hero = 0;
    std::string npc, slot;
    int def = 0, style = 0;
    // the item's effects are ours (the game makes none for an item giving the hero another
    // body: Razor's arcana Tormentor)
    bool ownGlows = false;
    bool operator==(const WearItem& o) const {
        return hero == o.hero && npc == o.npc && slot == o.slot && def == o.def && style == o.style && ownGlows == o.ownGlows;
    }
};
static std::vector<WearItem> g_wearItems;
// The preset's item models not merged into the hero's model (lines nomerge|model: their item
// says "skip_model_combine", and the mounted item table gives their slot's items the flag).
// A regular match's server knows nothing of the flag: it hides the slot's piece as merged,
// while this client's merged model leaves it out: The Barren Vector's claws were nowhere.
// Such a piece of your hero is shown again (LiveModelTick)
static std::set<std::string> g_noMerge;
static std::string g_dressedJs = "{}";  // their npc names, for the script (no glows of ours)
static std::set<std::string> g_dressedNpcs;  // the same, for the DLL (their skins are the game's)
// The preset's items' activity modifiers (lines actmod|npc|hero id|modifier|persona): the
// hero in the menu takes the stance they give (Legion Commander's arcana: "dualwield" and
// "arcana", both swords in hand). The game gathers a hero's modifiers from the items it
// dressed it in, and the preset's never are; a persona's only while it is shown.
struct ActMod {
    std::string npc;
    int hero = 0;
    std::string mod;
    bool persona = false;
    bool operator==(const ActMod& o) const { return npc == o.npc && hero == o.hero && mod == o.mod && persona == o.persona; }
};
static std::vector<ActMod> g_actMods;  // (g_personaLock)
static volatile LONG g_actModsCount = 0;

// (g_personaNpcs, g_personaShownNpcs: see PersonaLookOn)
// the hero you play, by its hero id, when it has a persona: its strategy screen shows ours
static volatile LONG g_localHeroId = 0;

static bool PersonaShownLocked(int hero) {  // (g_personaLock held)
    auto p = g_personaPicked.find(hero);
    return p != g_personaPicked.end() ? p->second.on : g_personaOn.count(hero) > 0;
}

// (see g_personaShownNpcs) After the preset, a switch or the hero you play changed.
static void UpdatePersonaShown() {
    std::set<std::string> all, shown;
    std::map<std::string, int> ids;
    AcquireSRWLockShared(&g_personaLock);
    for (auto& d : g_personaDefs) {
        if (d.second.npc.empty()) continue;
        all.insert(d.second.npc);
        ids[d.second.npc] = d.second.hero;
        if (PersonaShownLocked(d.second.hero)) shown.insert(d.second.npc);
    }
    ReleaseSRWLockShared(&g_personaLock);
    AcquireSRWLockExclusive(&g_rulesLock);
    g_personaNpcs.swap(all);
    g_personaShownNpcs.swap(shown);
    auto id = ids.find(g_localNpc);
    InterlockedExchange(&g_localHeroId, id != ids.end() ? id->second : 0);
    ReleaseSRWLockExclusive(&g_rulesLock);
}

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
// ParticleAttachment_t by name (the game's enum: PATTACH_ABSORIGIN = 0 …)
static int AttachType(const std::string& s) {
    static const char* const names[] = {
        "PATTACH_ABSORIGIN", "PATTACH_ABSORIGIN_FOLLOW", "PATTACH_CUSTOMORIGIN", "PATTACH_CUSTOMORIGIN_FOLLOW",
        "PATTACH_POINT", "PATTACH_POINT_FOLLOW", "PATTACH_EYES_FOLLOW", "PATTACH_OVERHEAD_FOLLOW",
        "PATTACH_WORLDORIGIN", "PATTACH_ROOTBONE_FOLLOW", "PATTACH_RENDERORIGIN_FOLLOW", "PATTACH_MAIN_VIEW",
        "PATTACH_WATERWAKE", "PATTACH_CENTER_FOLLOW", "PATTACH_CUSTOM_GAME_STATE_1", "PATTACH_HEALTHBAR"};
    for (int i = 0; i < (int)(sizeof(names) / sizeof(names[0])); i++)
        if (s == names[i]) return i;
    return 1;  // PATTACH_ABSORIGIN_FOLLOW
}

// glow|npc|particle|cp:type:attachment;...|item model|part,part (split already)
static GlowDef GlowDefOf(const std::vector<std::string>& f) {
    GlowDef d;
    d.npc = f[1];
    d.fx = Normalize(f[2]);
    d.item = f.size() > 4 ? Normalize(f[4]) : std::string();
    d.hasParts = f.size() > 5;
    if (d.hasParts)
        for (auto& p : Split(f[5], ','))
            if (!p.empty()) d.parts.push_back(Normalize(p));
    d.place = f.size() > 6 ? atoi(f[6].c_str()) : 0;
    for (auto& s : Split(f[3], ';')) {
        auto p = Split(s, ':');
        if (p.size() < 3 || p[0].empty()) continue;
        GlowCp c{};
        c.cp = atoi(p[0].c_str());
        c.attach = AttachType(p[1]);
        c.world = p[1] == "PATTACH_WORLDORIGIN";
        if (c.world) {
            auto v = Split(p[2], ',');
            for (int k = 0; k < 3; k++) c.vec[k] = v.size() == 3 ? (float)atof(v[k].c_str()) : 0.0f;
        } else {
            c.parent = !p[2].empty() && p[2][0] == '^';
            c.att = c.parent ? p[2].substr(1) : p[2];
        }
        d.cps.push_back(c);
    }
    return d;
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
    std::vector<GlowDef> glowDefs;             // the glows for the DLL's own match effects
    std::string attackFx;                      // the emblem's attack effect
    LONG weather = -1;                         // the weather type
    std::string terrainFrom, terrainTo, hudSkin;
    std::string shaders[kShaderKinds];
    std::unordered_map<std::string, std::string> matGroups;
    std::unordered_map<std::string, MeshMask> meshMasks;
    std::unordered_map<std::string, uint32_t> gems;
    std::string gemColors, gemPicks, gemItems, preset;
    std::map<std::string, PersonaDef> personaDefs;
    std::set<int> personaOn;
    std::vector<WearItem> wearItems;
    std::set<std::string> noMerge;
    std::vector<ActMod> actMods;
    std::string lang = "ru";
    while (std::getline(file, line)) {
        if (!line.empty() && line.back() == '\r') line.pop_back();
        if (line.empty() || line[0] == '#') continue;
        if (line.compare(0, 5, "lang|") == 0) { lang = line.substr(5); continue; }
        if (line.compare(0, 11, "personadef|") == 0) {
            // personadef|HERO|hero id|persona item|npc
            auto f = Split(line, '|');
            if (f.size() >= 4 && atoi(f[2].c_str()) > 0 && atoi(f[3].c_str()) > 0)
                personaDefs[f[1]] = {atoi(f[2].c_str()), atoi(f[3].c_str()), f.size() > 4 ? Normalize(f[4]) : std::string()};
            continue;
        }
        if (line.compare(0, 8, "persona|") == 0) {
            if (int hero = atoi(line.c_str() + 8)) personaOn.insert(hero);
            continue;
        }
        if (line.compare(0, 5, "wear|") == 0) {
            // wear|hero id|npc|slot|item|style|own glows (1)
            auto f = Split(line, '|');
            if (f.size() >= 6 && atoi(f[1].c_str()) > 0 && atoi(f[4].c_str()) > 0)
                wearItems.push_back({atoi(f[1].c_str()), f[2], f[3], atoi(f[4].c_str()), atoi(f[5].c_str()), f.size() > 6 && f[6] == "1"});
            continue;
        }
        if (line.compare(0, 8, "nomerge|") == 0) {
            noMerge.insert(Normalize(line.substr(8)));
            continue;
        }
        if (line.compare(0, 7, "actmod|") == 0) {
            // actmod|npc|hero id|modifier|persona (0/1)
            auto f = Split(line, '|');
            if (f.size() >= 5 && !f[1].empty() && !f[3].empty())
                actMods.push_back({f[1], atoi(f[2].c_str()), f[3], f[4] == "1"});
            continue;
        }
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
        if (line.compare(0, 8, "terrain|") == 0) {
            auto f = Split(line, '|');
            if (f.size() >= 3) {
                terrainFrom = Normalize(f[1]);
                terrainTo = Normalize(f[2]);
            }
            continue;
        }
        if (line.compare(0, 4, "gem|") == 0) {
            auto f = Split(line, '|');
            if (f.size() >= 3 && !f[1].empty() && f[2].size() == 6) {
                uint32_t rgb = (uint32_t)strtoul(f[2].c_str(), nullptr, 16);
                gems[Normalize(f[1])] = ((rgb >> 16) & 0xFF) | (rgb & 0xFF00) | ((rgb & 0xFF) << 16) | 0xFF000000u;
            }
            continue;
        }
        if (line.compare(0, 9, "gemcolor|") == 0) {
            auto f = Split(line, '|');
            if (f.size() >= 3 && !f[1].empty())
                gemColors += (gemColors.empty() ? "[" : ",[") + JsString(f[1]) + "," + JsString(f[2]) + "]";
            continue;
        }
        if (line.compare(0, 7, "picked|") == 0) {
            auto f = Split(line, '|');
            if (f.size() >= 2 && !f[1].empty())
                preset += (preset.empty() ? "[" : ",[") + JsString(f[1]) + "," + (f.size() >= 3 && !f[2].empty() ? f[2] : "0") + "]";
            continue;
        }
        if (line.compare(0, 8, "gemitem|") == 0) {
            auto f = Split(line, '|');
            if (f.size() >= 2 && !f[1].empty())
                gemItems += (gemItems.empty() ? "[" : ",[") + JsString(f[1]) + "," + JsString(f.size() >= 3 ? f[2] : "") + "]";
            continue;
        }
        if (line.compare(0, 8, "gempick|") == 0) {
            auto f = Split(line, '|');
            if (f.size() >= 3 && !f[1].empty()) gemPicks += (gemPicks.empty() ? "" : ",") + JsString(f[1]) + ":" + JsString(f[2]);
            continue;
        }
        if (line.compare(0, 9, "meshmask|") == 0) {
            auto f = Split(line, '|');
            if (f.size() >= 5 && !f[1].empty()) {
                MeshMask m{strtoull(f[2].c_str(), nullptr, 10), strtoull(f[3].c_str(), nullptr, 10),
                           strtoull(f[4].c_str(), nullptr, 10)};
                if (m.clear && m.set) meshMasks[Normalize(f[1])] = m;
            }
            continue;
        }
        if (line.compare(0, 9, "matgroup|") == 0) {
            auto f = Split(line, '|');
            if (f.size() >= 3 && !f[1].empty() && !f[2].empty()) matGroups[Normalize(f[1])] = f[2];
            continue;
        }
        if (line.compare(0, 7, "shader|") == 0) {
            auto f = Split(line, '|');
            int kind = f.size() < 3 ? -1 : f[1] == "map" ? kShaderMap : f[1] == "portrait" ? kShaderPortrait : f[1] == "post" ? kShaderPost : -1;
            if (kind >= 0) {
                shaders[kind] = Normalize(f[2]);
                // loaded in the menu beforehand: a match loads nothing it does not list
                if (!shaders[kind].empty()) glowFiles.insert(shaders[kind]);
            }
            continue;
        }
        if (line.compare(0, 8, "hudskin|") == 0) {
            hudSkin = Normalize(line.substr(8));
            continue;
        }
        if (line.compare(0, 8, "weather|") == 0) {
            weather = atoi(line.c_str() + 8);
            continue;
        }
        if (line.compare(0, 7, "attack|") == 0) {
            // loaded in the menu beforehand, as the glows' effects
            attackFx = Normalize(line.substr(7));
            if (!attackFx.empty()) glowFiles.insert(attackFx);
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
            auto f = Split(line, '|');
            // (the menu's script: not one the item spawns in the game only)
            std::string g = f.size() > 6 && f[6] == "2" ? std::string() : GlowJs(line);
            if (!g.empty()) glows += (glows.empty() ? "" : ",") + g;
            if (f.size() > 4 && !f[4].empty()) itemKeys.insert(Normalize(f[4]));
            // the effects the script creates itself, loaded in the menu beforehand: a
            // regular match loads nothing on demand (red crosses instead of them)
            if (f.size() > 2 && !f[2].empty()) glowFiles.insert(Normalize(f[2]));
            if (f.size() >= 4) glowDefs.push_back(GlowDefOf(f));
            if (f.size() > 5)
                for (auto& part : Split(f[5], ','))
                    if (!part.empty()) glowFiles.insert(Normalize(part));
            continue;
        }
        // dst|src, or dst|src|npc,npc for a hero item's swap (|match: a match's swap of
        // the file, |menu: the menu's only)
        auto rule = Split(line, '|');
        if (rule.size() >= 2) {
            RuleMode mode = rule.size() > 3 && rule[3] == "match" ? kRuleMatch
                          : rule.size() > 3 && rule[3] == "menu" ? kRuleMenu : kRuleAlways;
            AddRule(*fresh, rule[0], rule[1], rule.size() > 2 ? rule[2] : std::string(), mode);
        }
    }
    // an item model is worn under any model the swap table turns into it (the
    // slot's default one, the player's own item)
    std::unordered_map<std::string, std::string> itemModels;
    for (auto& k : itemKeys) itemModels[k] = k;
    for (auto* map : {&fresh->replacements, &fresh->matchReplacements})
        for (auto& r : *map)
            if (itemKeys.count(r.second)) itemModels.emplace(r.first, r.second);
    AcquireSRWLockExclusive(&g_rulesLock);
    g_lang = (lang == "en") ? "en" : "ru";
    Rules* old = g_rules;
    g_rules = fresh;
    ReleaseSRWLockExclusive(&g_rulesLock);
    AcquireSRWLockExclusive(&g_glowsLock);
    g_glowsJs = "[" + glows + "]";
    g_itemModels.swap(itemModels);
    g_glowDefs.swap(glowDefs);
    g_attackFx = attackFx;
    g_terrainFrom = terrainFrom;
    g_terrainTo = terrainTo;
    g_hudSkin = hudSkin;
    for (int k = 0; k < kShaderKinds; k++) g_shaders[k] = shaders[k];
    g_matGroups.swap(matGroups);
    g_meshMasks.swap(meshMasks);
    g_gems.swap(gems);
    g_gemColorsJs = "[" + gemColors + "]";
    g_gemPicksJs = "{" + gemPicks + "}";
    g_gemItemsJs = "[" + gemItems + "]";
    g_presetJs = "[" + preset + "]";
    InterlockedExchange(&g_weatherWanted, weather >= 0 && weather <= 9 ? weather : -1);
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
    {
        AcquireSRWLockExclusive(&g_personaLock);
        bool changed = personaDefs != g_personaDefs || personaOn != g_personaOn || wearItems != g_wearItems;
        g_personaDefs.swap(personaDefs);
        g_personaOn.swap(personaOn);
        g_wearItems.swap(wearItems);
        g_noMerge.swap(noMerge);
        g_actMods.swap(actMods);
        InterlockedExchange(&g_actModsCount, (LONG)g_actMods.size());
        // a switch on the page the preset now says too is the preset's again (one it does
        // not say, e.g. the launcher had not got to it yet, stays a while)
        ULONGLONG now = GetTickCount64();
        for (auto it = g_personaPicked.begin(); it != g_personaPicked.end();) {
            bool agrees = (g_personaOn.count(it->first) > 0) == it->second.on;
            if (agrees || now - it->second.at > 15000) {
                // (an expired one the preset does not say: the page shows the preset's again)
                if (!agrees) changed = true;
                it = g_personaPicked.erase(it);
            } else {
                ++it;
            }
        }
        ReleaseSRWLockExclusive(&g_personaLock);
        UpdatePersonaShown();
        if (changed) InterlockedIncrement(&g_personaVersion);
    }
    InterlockedIncrement(&g_glowsVersion);
    static bool first = true;
    if (!first) InterlockedExchange(&g_refreshPending, 1);
    first = false;
    // the originals of the swaps that go (an item taken out: Razor's arcana Tormentor, its
    // body, whip and robe), loaded before the preview is made again: handed out as their
    // replacement while the swap was on, they were never read (or let go since), and the
    // preview made again with them unloaded drew nothing of Razor but his armor and bracers
    std::set<std::string> preload = glowFiles;
    if (old)
        for (auto& r : old->replacements) {
            auto now = fresh->replacements.find(r.first);
            if ((now == fresh->replacements.end() || now->second != r.second) && IsSwappableResource(r.first.c_str()))
                preload.insert(r.first);
        }
    delete old;
    QueuePreloads(*fresh, preload);
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
    if (const std::string* to = RuleFor(rules, base, true)) {
        out = compiled ? *to + "_c" : *to;
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

// Which item each model was read with: model -> the model whose file it holds (itself
// when nothing was swapped). A match reads a model once; a pick made later changes the
// wearables' model instead (see LiveModelTick).
static SRWLOCK g_loadedAsLock = SRWLOCK_INIT;
static std::unordered_map<std::string, std::string> g_loadedAs;

// A resource's own file under another name: "dsl_own/<name>". A file read as an item's
// (the slot's default whip read as Razor's arcana Tormentor while the preset had it) stays
// so in memory under its own name when the item is taken out: the menu hands out this
// copy, read from the file itself, instead (see HookedFindOrRegister)
static const char kOwnPrefix[] = "dsl_own/";
static const size_t kOwnPrefixLen = sizeof(kOwnPrefix) - 1;

static void RememberRead(const char* fileName, const std::string& replacement) {
    std::string name = Normalize(fileName);
    if (name.size() < 7 || name.compare(name.size() - 2, 2, "_c") != 0 || name.compare(0, kOwnPrefixLen, kOwnPrefix) == 0) return;
    name.resize(name.size() - 2);
    if (!IsSwappableResource(name.c_str())) return;
    std::string held = replacement.empty() ? name : Normalize(replacement);
    if (held.size() > 2 && held.compare(held.size() - 2, 2, "_c") == 0) held.resize(held.size() - 2);
    AcquireSRWLockExclusive(&g_loadedAsLock);
    g_loadedAs[name] = held;
    ReleaseSRWLockExclusive(&g_loadedAsLock);
}

// Whether a resource's file was read as another one's (an item's), and still is in memory so
static bool ReadAsOther(const std::string& name) {
    AcquireSRWLockShared(&g_loadedAsLock);
    auto f = g_loadedAs.find(name);
    bool other = f != g_loadedAs.end() && f->second != name;
    ReleaseSRWLockShared(&g_loadedAsLock);
    return other;
}

static void HookedInitForFileRead(void* self, void* binding, uintptr_t priority, uintptr_t flag, const char* fileName) {
    std::string replacement;
    {
        // an own copy ("dsl_own/<name>"): the file of <name> itself
        std::string own;
        if (fileName && fileName[0]) {
            own = fileName;
        } else {
            StackBufferString name;
            auto getName = (*(GetFileNameFn**)g_ResourceSystem)[0x180 / 8];
            getName(g_ResourceSystem, binding, &name, 0);
            own = BufStr_Get(&name);
        }
        if (_strnicmp(own.c_str(), kOwnPrefix, kOwnPrefixLen) == 0) {
            std::string real = own.substr(kOwnPrefixLen);
            Log("[+] %s: its own file %s", own.c_str(), real.c_str());
            return g_OrigInitForFileRead(self, binding, priority, flag, real.c_str());
        }
    }
    if (fileName && fileName[0]) {
        bool swapped = FindReplacementFile(fileName, replacement);
        RememberRead(fileName, swapped ? replacement : std::string());
        if (swapped) {
            Log("[+] %s -> %s", fileName, replacement.c_str());
            return g_OrigInitForFileRead(self, binding, priority, flag, replacement.c_str());
        }
    } else {
        StackBufferString name;
        auto getName = (*(GetFileNameFn**)g_ResourceSystem)[0x180 / 8];
        getName(g_ResourceSystem, binding, &name, 0);
        const char* cur = BufStr_Get(&name);
        bool swapped = cur[0] && FindReplacementFile(cur, replacement);
        if (cur[0]) RememberRead(cur, swapped ? replacement : std::string());
        if (swapped) {
            Log("[+] %s -> %s", cur, replacement.c_str());
            return g_OrigInitForFileRead(self, binding, priority, flag, replacement.c_str());
        }
    }
    g_OrigInitForFileRead(self, binding, priority, flag, fileName);
}

// --- Preloading replacements for the menu ---
// The engine's resource name: a CBufferString with a 200-byte buffer, then a hash and a type.
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

// Every replacement of a new table is looked at again, the ones loaded before too: the
// game lets go of a resource nothing uses (a hero's classic items while the preset had its
// persona), and one let go was handed out by name no more (RunPreloads loads it again)
static void QueuePreloads(const Rules& rules, const std::set<std::string>& extra) {
    AcquireSRWLockExclusive(&g_preloadLock);
    for (auto& r : rules.replacements) {
        const std::string& t = r.second;
        if (IsSwappableResource(t.c_str())) g_preloadQueue.insert(t);
    }
    for (auto& t : extra)
        if (IsSwappableResource(t.c_str())) g_preloadQueue.insert(t);
    ReleaseSRWLockExclusive(&g_preloadLock);
    // files read as an item that no swap has now (the item taken out): their own copies (the
    // menu's: a match is given only what its manifest holds)
    if (!g_inMenu) return;
    std::vector<std::string> own;
    AcquireSRWLockShared(&g_loadedAsLock);
    for (auto& l : g_loadedAs)
        if (l.second != l.first && !rules.replacements.count(l.first)) own.push_back(kOwnPrefix + l.first);
    ReleaseSRWLockShared(&g_loadedAsLock);
    AcquireSRWLockExclusive(&g_preloadLock);
    for (auto& o : own) g_preloadQueue.insert(o);
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
        bool loaded = status(g_ResourceSystem, &name) == kResourceResident;
        if (!loaded) g_BlockingLoad(g_ResourceSystem, &name, 0);
        bool ok = loaded || status(g_ResourceSystem, &name) == kResourceResident;
        // (one still loaded is only noted)
        if (!loaded) Log("[%c] Preload %s", ok ? '+' : '-', model.c_str());
        AcquireSRWLockExclusive(&g_preloadLock);
        if (ok) g_resident.insert(model);
        else g_resident.erase(model);
        ReleaseSRWLockExclusive(&g_preloadLock);
    }
}


// --- Swap on resource lookup (menu) ---
// CResourceSystem::FindOrRegisterResourceByName_Internal(rs, ResourceName*, bool):
// gives a resource's binding by name and hash. In the menu, for an original whose
// replacement is loaded, the replacement's binding is given: as when an item is changed
// with Dota's «Equip», the panel gets another resource and redraws the model.
using FindOrRegisterFn = void* (*)(void* rs, ResourceName* name, uintptr_t flag);
static FindOrRegisterFn g_OrigFindOrRegister = nullptr;

// The game's main thread (the interface scripts' messages come on it)
static DWORD g_mainThread = 0;
static thread_local bool t_inJitLoad = false;

// An empty slot's stand-in model (our item table's): a regular match's server knows
// nothing of it, so it is in no manifest of the match and the wearable got the error
// model. Asked for in a match, it is loaded first into the engine's just-in-time manifest
// (what the engine does for an asset that was not precached): another player's hero gets
// the stand-in's own file (an invisible box: the slot stays empty), the hero you play the
// picked item (the swap reads its file instead).
static void LoadStandInNow(void* rs, ResourceName* name) {
    if (MenuShown() || t_inJitLoad || !g_BlockingLoad || GetCurrentThreadId() != g_mainThread) return;
    const char* cur = BufStr_Get(name);
    if (_strnicmp(cur, "models/items/dota_skins_slots/", 30) != 0) return;
    auto status = (GetStatusFn)(*(void***)rs)[kGetStatusSlot];
    if (status(rs, name) == kResourceResident) return;
    t_inJitLoad = true;
    g_BlockingLoad(rs, name, 0);
    t_inJitLoad = false;
    Log("[%c] Stand-in %s loaded for the match", status(rs, name) == kResourceResident ? '+' : '-', cur);
}

static bool PersonaModelFor(const char* name, std::string& out);

static void* HookedFindOrRegister(void* rs, ResourceName* name, uintptr_t flag) {
    if (name) LoadStandInNow(rs, name);
    // A match: a model of the hero you play that its persona (shown now) swaps, its body or a
    // classic piece, is handed out as the persona's, loaded with its original beforehand (see
    // PreloadPersonaModels). An entity made after the switch (the strategy screen's preview
    // made again, the hero at its spawn, its illusions) so wears the persona from the start,
    // its pieces on the persona's bones. (Never read into the files, and not put on live:
    // a body changed under its pieces left them on the old bones.)
    if (name && g_SetResourceName && !g_inMenu && !g_preGame && g_uiState != kUiLoading) {
        std::string repl;
        if (PersonaModelFor(BufStr_Get(name), repl)) {
            ResourceName other;
            if (g_SetResourceName(&other, repl.c_str())) {
                void* r = g_OrigFindOrRegister(rs, &other, flag);
                if (r) {
                    static std::set<std::string> noted;
                    static SRWLOCK notedLock = SRWLOCK_INIT;
                    std::string from = Normalize(BufStr_Get(name));
                    AcquireSRWLockExclusive(&notedLock);
                    bool first = noted.insert(from).second;
                    ReleaseSRWLockExclusive(&notedLock);
                    if (first) Log("[*] Persona model handed out: %s -> %s", from.c_str(), repl.c_str());
                    return r;
                }
            }
        }
    }
    // in the menu and in a match: a resource loaded before the pick (emblem ring, screen
    // shader) stays in the cache with the old swap otherwise
    if (name && g_SetResourceName) {
        const char* cur = BufStr_Get(name);
        size_t len = strlen(cur);
        std::string repl;
        // not effects in a match: a regular match gives only what its manifest holds, and
        // an item's effect asked for by its own name came back as the error effect (red
        // crosses); its data is swapped in by InitForFileRead under the original name anyway
        // (models too: the match's strategy screen got error models the same way). Not on a
        // match's loading screen either, though the script still says "menu" there: the
        // match registers its resources then (lane creeps), and an original handed out as
        // its replacement was never registered: "not in the system", the error model
        bool material = len > 5 && _stricmp(cur + len - 5, ".vmat") == 0;
        if (len > 5 && IsSwappableResource(cur) && (MenuShown() || material) && FindReplacementFile(cur, repl) && IsResident(repl)) {
            ResourceName other;
            if (g_SetResourceName(&other, repl.c_str())) {
                void* r = g_OrigFindOrRegister(rs, &other, flag);
                if (r) { InterlockedIncrement(&g_substituted); return r; }
            }
        } else if (len > 5 && IsSwappableResource(cur) && MenuShown() && _strnicmp(cur, kOwnPrefix, kOwnPrefixLen) != 0 &&
                   !FindReplacementFile(cur, repl)) {
            // no swap now, its file read as an item's before (the item taken out): its own copy
            std::string n = Normalize(cur);
            std::string own = kOwnPrefix + n;
            if (ReadAsOther(n) && IsResident(own)) {
                ResourceName other;
                if (g_SetResourceName(&other, own.c_str())) {
                    void* r = g_OrigFindOrRegister(rs, &other, flag);
                    if (r) { InterlockedIncrement(&g_substituted); return r; }
                }
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
if (typeof DSLTxt === "undefined") DSLTxt = { select: "SELECT", selected: "SELECTED", have: "OWNED", restart: "AFTER RESTART", gemOwn: "The item's own gem", gemNote: "Only you see the gem: the item on your account does not change." };
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
  var lastMenu = null, lastPreGame = null;
  var lastHero = "", lastHeroWhy = "", lastPicked = "";
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
  // Every item in a cosmetic picker: its «available to buy» and «unavailable» filters are
  // kept on (Dota shows owned items only, and every item is one of yours here). Kept, not
  // set once: the strategy screen's picker is the same panel match after match, and Dota
  // turns its filters back to «owned» for every new match.
  DSLAllItems = function (picker) {
    if (!picker || !picker.IsValid()) return;
    var changed = null;
    ["AvailableFilterButton", "UnavailableFilterButton"].forEach(function (id) {
      var b = picker.FindChildTraverse(id);
      if (b && !b.checked) { b.checked = true; changed = b; }
    });
    if (!changed) return;
    // what the buttons' own onactivate sends
    try { $.DispatchEvent("DOTACosmeticPickerFilterChanged", changed); } catch (e) { $.Msg("DSL_LOG filter error " + e); }
    if (picker.GetAttributeString("dsl_filters", "") !== "1") {
      picker.SetAttributeString("dsl_filters", "1");
      $.Msg("DSL_LOG every item shown in " + picker.id);
    }
  };
  // A persona switch (CLASSIC / persona). A persona you own switches as Dota's own. One you do
  // not: over its tab lies Dota's invisible button (on the hero page from 150px on), whose
  // press puts the persona on (its tab's own press too), and the classic tab takes it off:
  // the DLL shows it at once (the game then shows the persona as worn) and the launcher puts
  // it into the preset (or takes it out). On the hero page and on the strategy screen.
  DSLPersonaSwitch = function (sel, nameOf) {
    if (!sel || !sel.IsValid()) return;
    var pb = sel.FindChildTraverse("PersonaInvisibleButton");
    var db = sel.FindChildTraverse("DefaultCosmeticGroupButton");
    var pc = sel.FindChildTraverse("PersonaCosmeticGroupButton");
    if (!pb || !db) return;
    if (!sel.BHasClass("HasUsablePersonas") && !pb.enabled) pb.enabled = true;
    if (pb.GetAttributeString("dsl_persona", "") === "1") return;
    pb.SetAttributeString("dsl_persona", "1");
    var send = function (on) {
      var name = nameOf();
      $.Msg("DSL_LOG persona " + (on ? "on" : "off") + " for " + name);
      $.Msg("DSL_PICK 0|@persona\t" + name + "\t" + (on ? "on" : "off"));
    };
    pb.SetPanelEvent("onactivate", function () {
      if (!sel.BHasClass("HasUsablePersonas")) send(true);
    });
    if (pc) pc.SetPanelEvent("onactivate", function () {
      if (sel.BHasClass("HasUsablePersonas")) $.DispatchEvent("DOTAPersonaSelectorSelected", pc);
      else send(true);
    });
    db.SetPanelEvent("onactivate", function () {
      if (sel.BHasClass("HasUsablePersonas")) $.DispatchEvent("DOTAPersonaSelectorSelected", db);
      else send(false);
    });
  };
  // The hero of the page a card is on (the hero page, the strategy screen's picker)
  DSLPageHero = function (p) {
    var top = p;
    for (var u = 0; p && u < 60; u++, p = p.GetParent()) {
      top = p;
      if (p.paneltype === "DOTAHeroPageNewV2") {
        var hn = p.FindChildTraverse("HeroName");
        var lab = hn ? hn.FindChildrenWithClassTraverse("TitleExtraLarge")[0] : null;
        if (lab && lab.text) return lab.text;
      }
      if (p.paneltype === "DOTACosmeticPicker") {
        var title = p.FindChildrenWithClassTraverse("HeroTitle")[0];
        if (title && title.text) return title.text;
      }
    }
    var sel = top ? top.FindChildTraverse("SelectedHeroName") : null;
    return sel && sel.text ? sel.text : "";
  };
  // A hero's npc name. In a regular match the game gives no unit names (empty), so the
  // --- Prismatic gems: the item page's «Replace gem» opens every colour ---
  // The palette and the colours picked per item (by its name) come from the launcher
  // (DSLSetGems). A pick goes to it as DSL_PICK 0|@gem<TAB><colour or -><TAB><item's
  // names> (-: the item's own gem); it keeps it in the preset, and the DLL colours the hero.
  if (typeof DSLGems === "undefined") DSLGems = { colors: [], picks: {}, session: {}, items: [] };
  DSLSetGems = function (colors, picks, items) {
    DSLGems.colors = colors || [];
    DSLGems.picks = {};
    for (var k in picks || {}) DSLGems.picks[k.toLowerCase()] = picks[k];
    DSLGems.items = [];
    DSLGems.icons = {};
    (items || []).forEach(function (it) {
      var n = (it[0] || "").toLowerCase();
      DSLGems.items.push(n);
      DSLGems.icons[n] = it[1] || "";
    });
    if (!DSLGems.session) DSLGems.session = {};
  };
  DSLGemName = function (name) {
    var t = $.Localize("#" + name);
    return t && t.charAt(0) !== "#" ? t : name.replace(/^Unusual/, "").replace(/([a-z])([A-Z])/g, "$1 $2");
  };
  // the page's item: its title (a renamed item: its original name too)
  DSLGemPageNames = function (page) {
    var out = [];
    var titles = page.FindChildrenWithClassTraverse("ItemName");
    if (titles.length && titles[0].text) out.push(titles[0].text);
    var orig = page.FindChildTraverse("Name");
    if (orig && orig.paneltype === "Label" && orig.text && out.indexOf(orig.text) < 0) out.push(orig.text);
    return out;
  };
  // the colour picked for the item: this session's pick, else the preset's ("": its own gem)
  DSLGemPickOf = function (names) {
    for (var i = 0; i < names.length; i++) {
      var k = names[i].toLowerCase();
      if (DSLGems.session.hasOwnProperty(k)) return DSLGems.session[k];
    }
    for (var j = 0; j < names.length; j++) {
      var p = DSLGems.picks[names[j].toLowerCase()];
      if (p) return p;
    }
    return "";
  };
  // a socket holding a prismatic gem (its type line)
  DSLGemIsPrismatic = function (gem) {
    var types = gem.FindChildrenWithClassTraverse("GemType");
    var lab = types.length && types[0].GetChildCount() ? types[0].GetChild(0) : null;
    var text = lab && lab.text ? lab.text.toLowerCase() : "";
    if (!text) return false;
    var want = ($.Localize("#DOTA_WearableType_Prismatic_Gem") || "").toLowerCase();
    return (want && want.charAt(0) !== "#" && text.indexOf(want) >= 0) || text.indexOf("prismatic") >= 0 || text.indexOf("призмат") >= 0;
  };
  // the socket's gem name line («Dungeon Doom»)
  DSLGemNameLabel = function (gem) {
    var names = gem.FindChildrenWithClassTraverse("GemName");
    if (!names.length) return null;
    for (var c = names[0].GetChildCount() - 1; c >= 0; c--) {
      var l = names[0].GetChild(c);
      if (l.paneltype === "Label" && l.id !== "GemCount") return l;
    }
    return null;
  };
  // the picked colour's name in place of the item's gem (Dota reuses the page for other
  // items: its own text is kept per item, put back when the pick goes)
  DSLGemShowName = function (gem, item, name) {
    var l = DSLGemNameLabel(gem);
    if (!l) return;
    var mine = l.GetAttributeString("dsl_gem_item", "") === item && l.GetAttributeString("dsl_gem_set", "") !== "";
    if (!name) {
      if (mine) {
        l.text = l.GetAttributeString("dsl_gem_orig", l.text);
        l.SetAttributeString("dsl_gem_set", "");
      }
      return;
    }
    if (!mine) {
      l.SetAttributeString("dsl_gem_orig", l.text || "");
      l.SetAttributeString("dsl_gem_item", item);
      l.SetAttributeString("dsl_gem_set", "1");
    }
    var text = DSLGemName(name);
    if (l.text !== text) l.text = text;
  };
  // The picker as Dota's own («Select a gem»: PopupItemPicker with its choices, built from
  // its layout's styles: popups_shared, popup_item_picker; a choice's look is
  // DOTAItemPickerChoice's, a gem's icon the prismatic gem's: its setting and its stone in
  // the gem's colour)
  DSLGemIcon = function (parent, hex) {
    var box = $.CreatePanel("Panel", parent, "");
    box.style.width = "89px";
    box.style.height = "96px";
    box.style.margin = "6px";
    box.style.border = "2px solid black";
    box.style.backgroundColor = "black";
    box.style.boxShadow = "fill #000000aa 1px 0px 2px 2px";
    box.style.verticalAlign = "middle";
    var setting = $.CreatePanel("Image", box, "");
    setting.SetImage("s2r://panorama/images/econ/sockets/gem_color_png.vtex");
    var stone = $.CreatePanel("Image", box, "");
    stone.SetImage("s2r://panorama/images/econ/sockets/gem_color_mask_png.vtex");
    [setting, stone].forEach(function (img) {
      img.style.width = "100%";
      img.style.height = "100%";
      img.SetScaling("stretch-to-fit-preserve-aspect");
    });
    if (hex) stone.style.washColor = "#" + hex;
    else stone.style.saturation = "0";
    return box;
  };
  // Dota's PopupButton (.Green: the action), with the values of dotastyles.vcss: the
  // dashboard root our popup hangs on does not load that sheet
  DSLPopupButton = function (parent, text, green, small) {
    var b = $.CreatePanel("Button", parent, "");
    var idle = green ? "gradient( linear, 0% 0%, 0% 100%, from( #03380a ), to( #3d9b3d ) )"
                     : "gradient( linear, 0% 0%, 0% 100%, from( #222222 ), to( #4B555D ) )";
    var hover = green ? "gradient( linear, 0% 0%, 0% 100%, from( #085c13 ), to( #49a349 ) )"
                      : "gradient( linear, 0% 0%, 0% 100%, from( #58636e ), to( #53626e ) )";
    b.style.backgroundColor = idle;
    b.style.backgroundImage = "url('s2r://panorama/images/backgrounds/background_play_button_2x_png.vtex')";
    b.style.backgroundSize = "cover";
    b.style.border = green ? "1px solid #91EA4B40" : "1px solid #83838390";
    if (small) {
      b.style.minWidth = "0px";
      b.style.padding = "4px 10px 2px 10px";
      b.style.margin = "0px";
    } else {
      b.style.minWidth = "192px";
      b.style.padding = "10px 30px 10px 30px";
      b.style.margin = "8px";
    }
    b.SetPanelEvent("onmouseover", function () { b.style.backgroundColor = hover; });
    b.SetPanelEvent("onmouseout", function () { b.style.backgroundColor = idle; });
    var l = $.CreatePanel("Label", b, "");
    l.text = text;
    l.style.color = "white";
    l.style.fontSize = small ? "18px" : "20px";
    l.style.horizontalAlign = "center";
    l.style.textAlign = "center";
    l.style.verticalAlign = "middle";
    l.style.textTransform = "uppercase";
    l.style.textOverflow = "shrink";
    l.style.textShadow = "0px 0px 5px 3.0 #000000";
    l.style.fontWeight = "semi-bold";
    l.style.letterSpacing = "1px";
    return b;
  };
  DSLGemMenu = function (root, names, gem) {
    var old = root.FindChildTraverse("DSLGemPopup");
    if (old) old.DeleteAsync(0);
    // (Dota's script has no layout-from-string: the panels are made here, with the values of
    // popups_shared.vcss and popup_item_picker.vcss; PopupButton is in the global styles)
    var pop = $.CreatePanel("Panel", root, "DSLGemPopup");
    // over the dashboard (its pages and popups), dimmed behind as Dota's popups
    pop.hittest = true;
    pop.style.zIndex = "1000";
    pop.style.width = "100%";
    pop.style.height = "100%";
    pop.style.backgroundColor = "#000000b0";
    pop.SetPanelEvent("onactivate", function () {});
    var close = function () { pop.DeleteAsync(0); };
    var box = $.CreatePanel("Panel", pop, "DSLGemPopupBox");
    box.hittest = true;
    box.SetPanelEvent("onactivate", function () {});
    box.style.backgroundColor = "gradient( linear, 0% 0%, 0% 100%, from( #272d2eee ), to( #131617ee ) )";
    box.style.border = "1px solid #5e6869";
    box.style.padding = "32px 64px 32px 64px";
    box.style.minWidth = "450px";
    box.style.verticalAlign = "middle";
    box.style.horizontalAlign = "center";
    box.style.boxShadow = "black 0px 0px 8px 0px";
    box.style.flowChildren = "down";
    var title = $.CreatePanel("Label", box, "");
    title.text = $.Localize("#DOTA_SelectGem_Title");
    title.style.fontSize = "30px";
    title.style.fontWeight = "thin";
    title.style.color = "#afb4b4";
    title.style.marginBottom = "15px";
    title.style.horizontalAlign = "center";
    var icon = "";
    for (var k = 0; k < names.length && !icon; k++) icon = DSLGems.icons[names[k].toLowerCase()] || "";
    if (icon) {
      var header = $.CreatePanel("Image", box, "HeaderItemImage");
      header.SetImage(icon);
      header.SetScaling("stretch-to-fit-preserve-aspect");
      header.style.horizontalAlign = "center";
      header.style.height = "100px";
      header.style.width = "150px";
      header.style.marginBottom = "10px";
    }
    var desc = $.CreatePanel("Label", box, "Description");
    desc.text = DSLTxt.gemNote || "";
    desc.style.horizontalAlign = "center";
    desc.style.fontSize = "20px";
    desc.style.color = "#afb4b4";
    var list = $.CreatePanel("Panel", box, "Choices");
    list.style.backgroundColor = "black";
    list.style.flowChildren = "right-wrap";
    list.style.width = "771px";
    list.style.overflow = "squish scroll";
    list.style.maxHeight = "450px";
    list.style.marginTop = "12px";
    list.style.borderLeft = "8px solid black";
    list.style.borderTop = "8px solid black";
    list.style.borderBottom = "8px solid black";
    var row = $.CreatePanel("Panel", box, "");
    row.style.flowChildren = "right";
    row.style.horizontalAlign = "center";
    row.style.marginTop = "25px";
    row.style.marginBottom = "20px";
    var cancel = DSLPopupButton(row, $.Localize("#DOTA_Cancel"), false, false);
    cancel.SetPanelEvent("onactivate", close);
    var cur = DSLGemPickOf(names);
    var colors = DSLGems.colors.slice().sort(function (a, b) { return DSLGemName(a[0]) < DSLGemName(b[0]) ? -1 : 1; });
    var nameOf = $.Localize("#Econ_Socket_Color_Name");
    var add = function (key, hex, title) {
      var ch = $.CreatePanel("Panel", list, "");
      ch.style.flowChildren = "right";
      ch.style.width = "370px";
      ch.style.marginRight = "8px";
      ch.style.marginBottom = "8px";
      ch.style.border = key === cur ? "1px solid #ffffff40" : "1px solid #ffffff06";
      DSLGemIcon(ch, hex);
      var details = $.CreatePanel("Panel", ch, "");
      details.style.flowChildren = "down";
      details.style.verticalAlign = "middle";
      details.style.marginLeft = "2px";
      details.style.height = "100%";
      details.style.width = "fill-parent-flow(1.0)";
      var labels = $.CreatePanel("Panel", details, "");
      labels.style.marginTop = "10px";
      labels.style.marginRight = "8px";
      labels.style.height = "fill-parent-flow(1.0)";
      labels.style.flowChildren = "down";
      var nm = $.CreatePanel("Label", labels, "");
      nm.text = title;
      nm.style.fontSize = "20px";
      nm.style.color = "white";
      nm.style.maxHeight = "26px";
      nm.style.textOverflow = "shrink";
      var ty = $.CreatePanel("Label", labels, "");
      ty.html = true;
      ty.text = key ? ", <font color='#8847ff'>Mythical</font>" : "";
      var buttons = $.CreatePanel("Panel", details, "");
      buttons.style.flowChildren = "right";
      buttons.style.margin = "0px 8px 8px 0px";
      buttons.style.width = "100%";
      var space = $.CreatePanel("Panel", buttons, "");
      space.style.width = "fill-parent-flow(1.0)";
      var sel = DSLPopupButton(buttons, $.Localize("#DOTA_SelectGem_Action"), true, true);
      sel.SetPanelEvent("onactivate", function () {
        $.Msg("DSL_PICK 0|@gem\t" + (key || "-") + "\t" + names.join("\t"));
        $.Msg("DSL_LOG gem " + (key || "-") + " for " + names.join(" / "));
        for (var n = 0; n < names.length; n++) DSLGems.session[names[n].toLowerCase()] = key;
        DSLGemShowName(gem, names[0], key);
        close();
      });
    };
    add("", "", DSLTxt.gemOwn || "-");
    for (var i = 0; i < colors.length; i++) {
      var cn = DSLGemName(colors[i][0]);
      add(colors[i][0], colors[i][1], nameOf && nameOf.charAt(0) !== "#" ? nameOf.replace("%s1", cn) : cn);
    }
  };
  // The store page (an item you do not own: no sockets of its own) of an item whose gem
  // colours its hero: a «Rare sockets» section of ours, after the description
  DSLGemStoreTick = function (root) {
    // every visible section list (the store page's, full screen or in the item popup), up
    // to the page holding it: its title is the item's name
    var lists = root.FindChildrenWithClassTraverse("SectionList");
    for (var i = 0; i < lists.length; i++) {
      var list = lists[i];
      if (!list.IsValid() || !list.visible) continue;
      var page = list, title = null;
      for (var up = 0; page && up < 8 && !title; up++) {
        page = page.GetParent();
        title = page ? page.FindChildTraverse("Title") : null;
      }
      if (!page || !title) continue;
      if (page.FindChildTraverse("RareSocketList")) continue;  // the owned item's page has its own
      var names = [];
      ["Title", "ItemName"].forEach(function (id) {
        var l = page.FindChildTraverse(id);
        if (l && l.paneltype === "Label" && l.text && names.indexOf(l.text) < 0) names.push(l.text);
      });
      var offered = names.some(function (n) { return DSLGems.items.indexOf(n.toLowerCase()) >= 0; });
      var seenKey = page.paneltype + "|" + names.join("/") + "|" + offered;
      if (!DSLGemStoreTick.seen) DSLGemStoreTick.seen = {};
      if (!DSLGemStoreTick.seen[seenKey]) {
        DSLGemStoreTick.seen[seenKey] = true;
        $.Msg("DSL_LOG gem store page " + page.paneltype + "#" + page.id + ": " + names.join(" / ") + (offered ? " (gems)" : ""));
      }
      var sec = list.FindChildTraverse("DSLGemSection");
      if (!offered || !DSLGems.colors.length) {
        if (sec && sec.visible) sec.visible = false;
        continue;
      }
      if (!sec) {
        sec = $.CreatePanel("Panel", list, "DSLGemSection");
        sec.AddClass("Section");
        var title = $.CreatePanel("Panel", sec, "");
        title.AddClass("SectionTitle");
        var tl = $.CreatePanel("Label", title, "");
        tl.AddClass("LabelLarge");
        tl.AddClass("LightestGreyText");
        tl.text = $.Localize("#DOTA_econ_item_details_rare_sockets_header");
        var row = $.CreatePanel("Panel", sec, "");
        row.style.width = "100%";
        row.style.flowChildren = "right";
        row.style.marginTop = "10px";
        // the prismatic gem as Dota draws it: the setting, and over it its stone (a grey
        // picture) washed with the colour
        var sw = $.CreatePanel("Panel", row, "DSLGemSwatch");
        sw.style.width = "66px";
        sw.style.height = "44px";
        sw.style.verticalAlign = "center";
        sw.style.marginRight = "14px";
        var setting = $.CreatePanel("Image", sw, "");
        setting.SetImage("file://{images}/econ/sockets/gem_color.png");
        var stone = $.CreatePanel("Image", sw, "DSLGemStone");
        stone.SetImage("file://{images}/econ/sockets/gem_color_mask.png");
        [stone, setting].forEach(function (im) {
          im.SetScaling("stretch-to-fit-preserve-aspect");
          im.style.width = "100%";
          im.style.height = "100%";
        });
        var info = $.CreatePanel("Panel", row, "");
        info.style.width = "fill-parent-flow(1.0)";
        info.style.flowChildren = "down";
        info.style.verticalAlign = "center";
        var nm = $.CreatePanel("Label", info, "DSLGemNameText");
        nm.style.fontSize = "20px";
        nm.style.color = "#ffffff";
        var ty = $.CreatePanel("Label", info, "");
        ty.text = $.Localize("#DOTA_WearableType_Prismatic_Gem");
        ty.style.fontSize = "14px";
        ty.style.textTransform = "uppercase";
        ty.style.color = "#8d939c";
        var b = $.CreatePanel("Button", row, "DSLGemButton");
        ["ButtonBevel", "ButtonPrimary", "Blue"].forEach(function (c) { b.AddClass(c); });
        b.style.verticalAlign = "center";
        var bl = $.CreatePanel("Label", b, "");
        bl.text = $.Localize("#DOTA_econ_item_details_replace_gem");
        var desc = list.FindChildrenWithClassTraverse("Description");
        if (desc.length) list.MoveChildAfter(sec, desc[0]);
        $.Msg("DSL_LOG gem section added for " + names[0]);
      }
      if (!sec.visible) sec.visible = true;
      var key = names.join("\t");
      var btn = sec.FindChildTraverse("DSLGemButton");
      if (btn && btn.GetAttributeString("dsl_gem_item", "") !== key) {
        btn.SetAttributeString("dsl_gem_item", key);
        (function (s, n) { btn.SetPanelEvent("onactivate", function () { DSLGemMenu(root, n, s); }); })(sec, names.slice());
      }
      var pick = DSLGemPickOf(names);
      var hex = "";
      for (var c = 0; c < DSLGems.colors.length; c++) if (DSLGems.colors[c][0] === pick) hex = DSLGems.colors[c][1];
      var nameText = pick ? DSLGemName(pick) : (DSLTxt.gemOwn || "-");
      var nl = sec.FindChildTraverse("DSLGemNameText");
      if (nl && nl.text !== nameText) nl.text = nameText;
      var stone = sec.FindChildTraverse("DSLGemStone");
      if (stone) {
        var wash = hex ? "#" + hex : "none";
        if (stone.GetAttributeString("dsl_wash", "") !== wash) {
          stone.SetAttributeString("dsl_wash", wash);
          stone.style.washColor = hex ? "#" + hex : null;
        }
      }
    }
  };
  // the item page: our «Replace gem» over Dota's for a prismatic socket (Dota's needs a
  // gem of yours and asks the item server to swap it)
  DSLGemTick = function (root) {
    if (!DSLGems.colors.length) return;
    // every socket of every item page (Dota keeps more than one)
    var gems = root.FindChildrenWithClassTraverse("GemContainer");
    var seen = gems.length;
    for (var i = 0; i < gems.length; i++) {
      var gem = gems[i];
      if (!gem.IsValid() || !DSLGemIsPrismatic(gem)) continue;
      var page = gem;
      for (var up = 0; page && up < 14 && page.id !== "ItemEditBackground"; up++) page = page.GetParent();
      if (!page) continue;
      var names = DSLGemPageNames(page);
      if (!names.length) {
        if (!DSLGemTick.noName) { DSLGemTick.noName = true; $.Msg("DSL_LOG gem: no item name on the item page"); }
        continue;
      }
      var key = names.join("	");
      var replace = gem.FindChildTraverse("ReplaceGem");
      var ours = gem.FindChildTraverse("DSLGemButton");
      if (!ours) {
        var parent = replace ? replace.GetParent() : gem;
        ours = $.CreatePanel("Button", parent, "DSLGemButton");
        ["ButtonBevel", "ButtonPrimary", "Blue"].forEach(function (c) { ours.AddClass(c); });
        var l = $.CreatePanel("Label", ours, "");
        l.text = $.Localize("#DOTA_econ_item_details_replace_gem");
        if (replace) parent.MoveChildBefore(ours, replace);
        $.Msg("DSL_LOG gem button added for " + names[0] + " (" + seen + " sockets)");
      }
      if (ours.GetAttributeString("dsl_gem_item", "") !== key) {
        ours.SetAttributeString("dsl_gem_item", key);
        (function (g, n) { ours.SetPanelEvent("onactivate", function () { DSLGemMenu(root, n, g); }); })(gem, names.slice());
      }
      if (replace && replace.visible) replace.visible = false;
      if (!ours.visible) ours.visible = true;
      // Dota disables the socket without gems of yours (and again when it redraws the page):
      // our button works anyway
      for (var en = ours; en && en !== gem.GetParent(); en = en.GetParent()) if (!en.enabled) en.enabled = true;
      DSLGemShowName(gem, names[0], DSLGemPickOf(names));
    }
  };
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
      // the strategy screen's «Loadout» tabs: the hero page's cosmetic picker in the HUD
      if (loopKind === "hud") {
        ["HeroCosmeticPicker", "GlobalCosmeticPicker"].forEach(function (id) {
          var pk = root.FindChildTraverse(id);
          if (pk) pages.push(pk);
        });
      }
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
        DSLAllItems(hp.paneltype === "DOTACosmeticPicker" ? hp : hp.FindChildTraverse("CosmeticPicker"));
        // item cards: each has an ItemInfoContainer (unavailable ones have no buy button)
        var stack = [hp];
        var infos = [];
        while (stack.length) {
          var p = stack.pop();
          if (p.id === "ItemInfoContainer") { infos.push(p); continue; }
          for (var c = 0; c < p.GetChildCount(); c++) stack.push(p.GetChild(c));
        }
        var added = 0;
        // does the list hold one of the preset's items (then it, not the account's
        // equipped item, is the «SELECTED» one)
        var listHasPreset = false;
        for (var k0 = 0; k0 < infos.length && !listHasPreset; k0++) listHasPreset = DSLInPreset(infos[k0].GetParent());
        // the preset's item of this slot further down the list (its card not made yet:
        // the default whip said «SELECTED» and could not be picked with the arcana in the
        // preset): the slot tiles' table says the preset has one
        if (!listHasPreset && infos.length && typeof DSLPresetSlotHas === "function") listHasPreset = DSLPresetSlotHas(infos[0].GetParent());
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
          // Dota's own status text on an owned card (before ours)
          var lab = owned && nativeStatus ? nativeStatus.FindChild("StatusLabel0") : null;
          if (lab) {
            var mine = lab.GetAttributeString("dsl_set", "");
            // (Dota may give the label its token first: «#DOTA_Loadout_Equipped»)
            var seen = lab.text || "";
            if (seen.charAt(0) === "#") seen = $.Localize(seen);
            if (lab.text !== mine && seen !== mine) lab.SetAttributeString("dsl_orig", seen);
          }
          var orig = lab ? lab.GetAttributeString("dsl_orig", "") : "";
          var origSelected = orig === DSLTxt.selected || /EQUIPPED/i.test(orig);
          // an item of yours while the preset has another one in this list (a slot's
          // default item among them): ours picks it into the preset, as on a card you do
          // not own (Dota's own press equips it on the account, or nothing when it is worn
          // already); a default item takes the preset's item of the slot out
          var wornHere = owned && listHasPreset && !DSLInPreset(row);
          var pickable = !owned || wornHere;
          var wrapHave = box.FindChild("DSLPickWrap");
          if (wrapHave) wrapHave.visible = pickable;
          if (nativeStatus && nativeStatus.GetAttributeString("dsl_hidden", "") === "1" && !pickable) {
            nativeStatus.visible = true;
            nativeStatus.SetAttributeString("dsl_hidden", "");
          }
          if (!pickable) {
            // Dota's status on an owned card («SELECTED» on the account's equipped item):
            // the preset's item is the selected one, the account's other items «OWNED»
            if (lab) {
              var want = DSLInPreset(row) ? DSLTxt.selected : (listHasPreset && origSelected ? DSLTxt.have : orig);
              if (want && want.charAt(0) === "#") want = $.Localize(want);
              if (want && lab.text !== want) {
                lab.text = want;
                lab.SetAttributeString("dsl_set", want);
              }
            }
            continue;
          }
          // hide the buy block with the price; on unavailable cards Dota's own «UNAVAILABLE» and the dimming
          var multi = box.FindChild("AvailableItemPurchaseButton");
          if (multi) multi.visible = false;
          if ((unavailable || wornHere) && nativeStatus) { nativeStatus.visible = false; nativeStatus.SetAttributeString("dsl_hidden", "1"); }
          card.style.opacity = "1";
          card.style.brightness = "1";
          var chain = [row.paneltype + "#" + row.id];
          DSLEnableStyles(box);
          var have = box.FindChildTraverse("DSLPickButton");
          if (have) { DSLSetPicked(have, have.GetChild(0), DSLIsPicked(row)); continue; }
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
          DSLSetPicked(b, l, DSLIsPicked(row));
          (function (b, l, row) {
            b.SetPanelEvent("onactivate", function () {
              if (!row || !row.IsValid()) return;
              var rowLabels = DSLPickLabels(row);
              // the page's hero: a default item is known by its name and hero only
              var pageHero = DSLPageHero(row);
              if (pageHero) rowLabels.push("@hero=" + pageHero);
              var key = DSLKey(row);
              if (DSLIsPicked(row)) return;
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
                var orow = DSLRowOf(ob);
                delete picked[DSLKey(orow)];
                var oname = orow ? DSLCardName(orow) : "";
                if (oname) delete presetNames[oname.toLowerCase()];
                DSLSetPicked(ob, ob.GetChild(0), false);
              }
              picked[key] = true;
              var myName = DSLCardName(row);
              if (myName) presetNames[myName.toLowerCase()] = true;
              b.SetAttributeInt("dsl_style", DSLStyleOf(row).style);
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
      // a match before its game starts (pick, strategy): the strategy screen's persona switch
      if (!inMenu) {
        var preGame = true;
        try { preGame = Game.GameStateIsBefore(DOTA_GameState.DOTA_GAMERULES_STATE_PRE_GAME); } catch (eG) {}
        if (preGame !== lastPreGame) { lastPreGame = preGame; $.Msg("DSL_PREGAME " + (preGame ? 1 : 0)); }
      } else {
        lastPreGame = null;
      }
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
      // the persona switch (CLASSIC / persona). A persona you own switches as Dota's own.
      // One you do not: over its tab lies Dota's invisible button (from 150px on), whose
      // press puts the persona on, and the classic tab takes it off: the DLL shows it at
      // once (the game then shows the persona as worn) and the launcher puts it into the
      // preset (or takes it out)
      if (inMenu) {
        try {
          var hpp = root.FindChildrenWithClassTraverse("DashboardPage");
          for (var pi2 = 0; pi2 < hpp.length; pi2++) {
            var hpg = hpp[pi2];
            if (hpg.paneltype !== "DOTAHeroPageNewV2" || !hpg.visible) continue;
            (function (page) {
              var nameOf = function () {
                var hn = page.FindChildTraverse("HeroName");
                var lab = hn ? hn.FindChildrenWithClassTraverse("TitleExtraLarge")[0] : null;
                return lab ? lab.text : "";
              };
              DSLPersonaSwitch(page.FindChildTraverse("PersonaSelector"), nameOf);
              DSLPersonaSwitch(page.FindChildTraverse("PersonaSelector2"), nameOf);
            })(hpg);
          }
        } catch (e12) { $.Msg("DSL_LOG persona error " + e12); }
        try { DSLGemTick(root); DSLGemStoreTick(root); } catch (e13) { $.Msg("DSL_LOG gem error " + e13); }
      } else if (loopKind === "hud") {
        // the strategy screen's switch (the hero you play)
        try {
          var strat = root.FindChildTraverse("StrategyScreen");
          if (strat) DSLPersonaSwitch(strat.FindChildTraverse("PersonaSelector"), function () {
            var sn = strat.FindChildTraverse("SelectedHeroName");
            return sn ? sn.text : "";
          });
        } catch (e14) { $.Msg("DSL_LOG strategy persona error " + e14); }
      }
      // item glows: on your own hero in a match, on the hero models in the menu
      $.Msg("DSL_GTICK " + loopKind);
      // glows from one loop: the menu's in the menu, the HUD's in a match (the other one
      // forgets its effects: their models go away with the map change)
      // (in a match the DLL creates them: the game gives scripts no entities there)
      if (loopKind === "dash" && inMenu) DSLGlowTick(inMenu); else DSLForgetGlows();
      if (inMenu || loopKind === "hud") DSLSlotTick();
      var log = types.filter(function (t) { return t.indexOf("DOTA") !== 0; }).join(", ");
      if (log && log !== last) { last = log; $.Msg("DSL_LOG " + log); }
    } catch (e) {
      $.Msg("DSL_LOG error " + e);
    }
    $.Schedule(0.5, tick);
  }
  // the row's item definition index: a property of the DOTAEconItem element
  DSLItemDef = function (row) {
    var card = row;
    for (var up = 0; card && up < 6 && card.paneltype !== "DOTAEconItemAnimated"; up++) card = card.GetParent();
    var stack = card && card.paneltype === "DOTAEconItemAnimated" ? [row, card] : [row], seen = [];
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
  // the row's key: its texts without the style switch («2 / 3»), so a picked card stays
  // picked when its style changes
  DSLKey = function (row) {
    if (!row || !row.IsValid()) return "";
    var out = [], stack = [row];
    while (stack.length) {
      var p = stack.pop();
      if (p.id === "DSLPickButton" || p.id === "StylesPanel") continue;
      if (p.paneltype === "Label" && p.text) out.push(p.text.replace(/[|\t\r\n]/g, " "));
      for (var c = p.GetChildCount() - 1; c >= 0; c--) stack.push(p.GetChild(c));
    }
    return out.join("|");
  };
  // the style shown on the row's card (from 0) and how many it has: its dots (radio
  // buttons), else the arrows' «2 / 3»; style -1 without styles
  DSLStyleOf = function (row) {
    var out = { style: -1, count: 0 };
    if (!row || !row.IsValid()) return out;
    var sel = row.FindChildTraverse("StyleSelector");
    if (sel) {
      for (var i = 0; i < sel.GetChildCount(); i++) {
        var r = sel.GetChild(i);
        if (r.paneltype !== "RadioButton") continue;
        if (r.checked) out.style = out.count;
        out.count++;
      }
    }
    if (out.count < 2 || out.style < 0) {
      var lab = row.FindChildTraverse("StyleIndexLabel");
      var m = lab && lab.text ? /(\d+)\s*\/\s*(\d+)/.exec(lab.text) : null;
      out = m && +m[2] > 1 ? { style: +m[1] - 1, count: +m[2] } : { style: -1, count: 0 };
    }
    return out;
  };
  // the row's texts for the launcher, the card's style first as «2 / 3» (the launcher
  // takes the first such text)
  DSLPickLabels = function (row) {
    var labels = DSLLabels(row), st = DSLStyleOf(row);
    if (st.style >= 0) labels.unshift((st.style + 1) + " / " + st.count);
    return labels;
  };
  // a picked card whose style is switched (its dots or arrows): the item is picked again
  // in that style. Per card (the button keeps what it saw): a card Dota builds anew shows
  // its first style without that being a pick
  DSLWatchStyle = function (row, b) {
    var now = Date.now();
    // the cursor on the card's style switch (its dots, its arrows): a change soon after
    // is yours; any other is Dota redrawing the card (after our refresh, a scroll of the
    // list...), which shows the item's first style
    var sp = row.FindChildTraverse("StylesPanel");
    if (sp && sp.BHasHoverStyle()) b.SetAttributeString("dsl_style_hover", "" + now);
    var st = DSLStyleOf(row);
    if (st.style < 0) return;
    var seen = b.GetAttributeInt("dsl_style", -2);
    var want = DSLPresetStyle(row);
    var yours = now - (+b.GetAttributeString("dsl_style_hover", "0")) < 1500;
    if (seen === st.style && (yours || want < 0 || st.style === want)) return;
    if (!yours) {
      // not a pick: the card shows the preset's style again (tried once a second)
      b.SetAttributeInt("dsl_style", st.style);
      if (want >= 0 && want < st.count && st.style !== want && now - (+b.GetAttributeString("dsl_style_fix", "0")) > 1000) {
        b.SetAttributeString("dsl_style_fix", "" + now);
        DSLShowStyle(row, want);
      }
      return;
    }
    b.SetAttributeInt("dsl_style", st.style);
    if (seen === -2) return;
    $.Msg("DSL_PICK " + DSLItemDef(row) + "|" + DSLPickLabels(row).join("	"));
    $.Msg("DSL_LOG style " + (st.style + 1) + " of " + st.count + " picked");
  };
  // the card's style switch for an item you do not own as well
  DSLEnableStyles = function (box) {
    var sp = box.FindChild("StylesPanel");
    if (!sp) return;
    var stack = [sp];
    while (stack.length) {
      var p = stack.pop();
      if ((p.paneltype === "RadioButton" || p.paneltype === "Button") && !p.enabled) p.enabled = true;
      for (var c = 0; c < p.GetChildCount(); c++) stack.push(p.GetChild(c));
    }
  };
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
  if (typeof DSLLastRefresh === "undefined") DSLLastRefresh = 0;  // our last preview refresh
  // the preset's items by name (from the launcher): their cards are the «SELECTED» ones
  var presetNames = {}, presetStyles = {};
  DSLSetPreset = function (list) {
    // the preset applied is what is picked now: a press's own mark goes (a default item's
    // stayed «SELECTED» after the arcana was picked again, and could not be picked)
    picked = {};
    presetNames = {};
    presetStyles = {};
    (list || []).forEach(function (it) {
      var n = ("" + it[0]).toLowerCase();
      presetNames[n] = true;
      presetStyles[n] = +it[1] || 0;
    });
  };
  // the preset's style of the row's item (-1: not in the preset)
  DSLPresetStyle = function (row) {
    var n = row ? DSLCardName(row).toLowerCase() : "";
    return n && presetNames[n] ? presetStyles[n] : -1;
  };
  // the card shows the style: its dot, else its arrows
  DSLShowStyle = function (row, style) {
    var sel = row.FindChildTraverse("StyleSelector");
    var radios = [];
    if (sel) for (var i = 0; i < sel.GetChildCount(); i++) if (sel.GetChild(i).paneltype === "RadioButton") radios.push(sel.GetChild(i));
    if (radios.length > style) {
      $.DispatchEvent("Activated", radios[style], "mouse");
      return;
    }
    var st = DSLStyleOf(row);
    var large = row.FindChildTraverse("LargeStyleSelector");
    if (!large || st.style < 0) return;
    var arrows = large.FindChildrenWithClassTraverse(style > st.style ? "Right" : "Left");
    for (var s = Math.abs(style - st.style); arrows.length && s > 0; s--) $.DispatchEvent("Activated", arrows[0], "mouse");
  };
  DSLCardName = function (row) {
    var card = row;
    while (card && card.paneltype !== "DOTAEconItemAnimated") card = card.GetParent();
    var l = card ? card.FindChildTraverse("EconItemName") : null;
    return l && l.text ? l.text : "";
  };
  DSLInPreset = function (row) {
    var n = row ? DSLCardName(row) : "";
    return !!n && !!presetNames[n.toLowerCase()];
  };
  DSLIsPicked = function (row) { return !!picked[DSLKey(row)] || DSLInPreset(row); };
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
  // Whether the preset has an item in the slot of a card (its caption among the card's labels,
  // the hero the page's)
  DSLPresetSlotHas = function (row) {
    var map = pendingSlots || slotMap;
    var hero = map ? map[slotKey(DSLPageHero(row))] : null;
    if (!hero) return false;
    var labels = DSLLabels(row);
    for (var i = 0; i < labels.length; i++) {
      var k = slotKey(labels[i]);
      if (k && hero[k] && hero[k][0] > 0) return true;
    }
    return false;
  };
  DSLSetSlots = function (m) {
    pendingSlots = m || {};
    $.Schedule(0, function () { DSLSlotTick(); });
  };
  // the heroes the DLL shows in their persona ({HERO: 1}): their tiles are the persona's
  // slots, given as "PERSONA"+caption and #P/#Q positions
  if (typeof DSLPersonas === "undefined") DSLPersonas = {};
  DSLSlotForce = function () { slotForceNext = true; slotWatch(1500); };
  function personaShown(hp, heroName) {
    if (DSLPersonas[slotKey(heroName)]) return true;
    // a persona you own and wear (Dota's own switch)
    var ids = ["PersonaSelector2", "PersonaSelector"];
    for (var i = 0; i < ids.length; i++) {
      var sel = hp.FindChildTraverse(ids[i]);
      if (!sel || !sel.BHasClass("HasMultiplePersonas") || !sel.BHasClass("HasUsablePersonas")) continue;
      var pc = sel.FindChildTraverse("PersonaCosmeticGroupButton");
      if (pc && pc.checked) return true;
    }
    return false;
  }
  var personaLogged = {};
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
    // the «equipped» corner (#Equipped, top right): the item's rarity, as Dota's own for
    // an immortal or an arcana (their images), a wash of its colour otherwise. Dota shows
    // it only on a tile of an equipped item (.Equipped), which ours is not: shown inline
    var corner = econ.FindChild("Equipped");
    if (corner && corner.style.visibility !== "visible") corner.style.visibility = "visible";
    if (corner && color && corner.GetAttributeString("dsl_color", "") !== m[3]) {
      corner.SetAttributeString("dsl_color", m[3]);
      var own = m[3] === "immortal" || m[3] === "arcana";
      corner.style.backgroundImage = "url('s2r://panorama/images/status_icons/econtooltip_equipped" + (own ? "_" + m[3] : "") + "_psd.vtex')";
      corner.style.washColor = own ? "none" : color;
      corner.style.width = own ? "23%" : "20%";
      corner.style.height = own ? "21%" : "19%";
    }
    var icon = m[2];
    if (!icon) {
      // an item in a style: Dota's image of it (set with its style) shows; the icon of
      // the item the tile had before (Crimson Witness under Bracers of Aeons) goes
      var stale = econ.FindChild("DSLSlotIcon");
      if (stale && stale.visible) stale.visible = false;
      return;
    }
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
    // the hero page (menu), and the strategy screen's «Loadout» picker (match HUD)
    var pages = root.FindChildrenWithClassTraverse("DashboardPage").filter(function (pg) {
      return pg.paneltype === "DOTAHeroPageNewV2" && pg.visible;
    });
    if (loopKind === "hud") {
      var pk = root.FindChildTraverse("HeroCosmeticPicker");
      if (pk && pk.visible) pages.push(pk);
    }
    var shown = [];
    for (var i = 0; i < pages.length; i++) {
      var hp = pages[i];
      var picker = hp.paneltype === "DOTACosmeticPicker";
      var nameP = picker ? null : hp.FindChildTraverse("HeroName");
      var heroName = nameP ? firstLabel(nameP) : "";
      if (picker) {
        // the picker's title (customize_hero_title), else the strategy screen's hero name
        var title = hp.FindChildrenWithClassTraverse("HeroTitle")[0];
        heroName = title && title.text ? title.text : "";
        if (!heroName) { var sel = root.FindChildTraverse("SelectedHeroName"); heroName = sel ? sel.text : ""; }
      }
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
      var persona = false;
      try { persona = personaShown(hp, heroName); } catch (ep) {}
      var cap = persona ? "PERSONA" : "";
      if (heroName !== slotHero) { slotHero = heroName; force = true; }
      // the panels whose look tells whether the wardrobe is open (watched by slotViewTick)
      if (!picker && slotViewPage !== hp) {
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
        var m = hero ? hero[cap + slotKey(labels[k].text)] : null;
        if (!m) {
          if (econ.GetAttributeInt("dsl_def", 0)) slotUndress(econ);
          continue;
        }
        shown.push({ econ: econ, m: m });
        try { slotSet(econ, m); }
        catch (e) { if (!slotErr) { slotErr = true; $.Msg("DSL_LOG slot icon error " + e); } }
      }
      // the "equipped" row: DOTAEconItem tiles without captions, in the hero
      // script's slot order (only when the count matches). A persona's row: its slots
      // shown in the loadout (#P), else all its item slots (#Q), whichever count matches
      for (var w = 0; w < rows.length; w++) {
        var tiles = [];
        for (var c2 = 0; c2 < rows[w].GetChildCount(); c2++)
          if (rows[w].GetChild(c2).paneltype === "DOTAEconItem") tiles.push(rows[w].GetChild(c2));
        var orders = persona ? ["#P", "#Q"] : ["#"];
        var order = null;
        for (var oi = 0; oi < orders.length && !order; oi++) {
          var len = hero ? hero[orders[oi] + "LEN"] : null;
          if (len && tiles.length === len[0]) order = orders[oi];
        }
        if (persona && !personaLogged[heroName + tiles.length]) {
          personaLogged[heroName + tiles.length] = true;
          $.Msg("DSL_LOG persona row of " + heroName + ": " + tiles.length + " tiles, " + (order ? "order " + order : "no order of that length"));
        }
        var fits = !!order;
        for (var r = 0; r < tiles.length; r++) {
          var mr = fits ? hero[order + r] : null;
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
    if (loopKind === "dash") slotTilesHero = slotHeroLabel && slotHeroLabel.IsValid() ? slotHeroLabel.text : "";
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
    // a card Dota gave another item (it reuses its cards when the list changes): that
    // item's state at once, not the previous one's «SELECTED» until the next slow tick
    for (var n2 = 0; n2 < alive.length; n2++) {
      var it2 = alive[n2];
      var nameLab = it2.row.FindChildTraverse("EconItemName");
      var shownName = nameLab && nameLab.text ? nameLab.text : "";
      if (it2.b.GetAttributeString("dsl_name", "") === shownName) continue;
      it2.b.SetAttributeString("dsl_name", shownName);
      it2.b.SetAttributeInt("dsl_style", -2);  // its style switch is seen anew
      DSLSetPicked(it2.b, it2.b.GetChild(0), DSLIsPicked(it2.row));
    }
    // picked cards: their style switch picks the item again in the new style
    for (var w = 0; w < alive.length; w++) {
      if (alive[w].b.GetAttributeString("dsl_state", "") === "on") DSLWatchStyle(alive[w].row, alive[w].b);
    }
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
    // the hero you picked, as soon as you pick: in the match the hero items' swaps apply
    // to it only (its models load right away)
    if (lastMenu === false) {
      try {
        var pickedHero = Players.GetPlayerSelectedHero ? Players.GetPlayerSelectedHero(Players.GetLocalPlayer()) : "";
        if (pickedHero && pickedHero !== lastPicked && pickedHero.indexOf("npc_dota_hero_") === 0) { lastPicked = pickedHero; $.Msg("DSL_PICKED " + pickedHero); }
      } catch (ePick) {}
    } else if (lastMenu) {
      lastPicked = "";
    }
    if (lastMenu) { slotHeroTick(); slotViewTick(); slotRepair(); }
    else if (loopKind === "hud") slotRepair();
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
  // (In a regular match Entities.IsValidEntity says "not valid" even for your own live
  // hero and its items: there the DLL's list, read from the entity list just now, decides.)
  var glowInMatch = false;
  function hostOf(gl, ent) {
    var l = gl.item ? itemHosts[gl.item] : null;
    for (var i = 0; l && i < l.length; i++) {
      if ((l[i][2] === ent || l[i][3] === ent) && (glowInMatch || Entities.IsValidEntity(l[i][0]))) return l[i];
    }
    return null;
  }
  // An effect is removed through the API only while its entity is alive: when a map loads
  // («Try out»), the menu models are deleted, and a call on a dead entity crashes the game.
  // The game removes a deleted entity's effects itself: they are just forgotten. (An item
  // model lives while the DLL lists its handle: its index may belong to a new entity.)
  function dropGlow(k) {
    var g = glowFx[k];
    var alive = g && (g.handle >= 0 ? hostListed(g.handle) && (glowInMatch || Entities.IsValidEntity(g.ent))
                                     : Entities.IsValidEntity(g.ent) || (glowInMatch && g.ent === DSLLocalHero()));
    if (alive) {
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
  var glowHostLogged = {}, glowDiag = "";
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
    glowInMatch = !inMenu;
    var alive = {};
    if (glowList.length) {
      var wanted = {};
      for (var g0 = 0; g0 < glowList.length; g0++) {
        // (in the menu not the heroes the game dresses itself: it makes their effects)
        if (inMenu && typeof DSLDressed !== "undefined" && DSLDressed[glowList[g0].npc]) continue;
        wanted[glowList[g0].npc] = true;
      }
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
        var heroValid = hero > 0 && Entities.IsValidEntity(hero);
        var heroName = hero > 0 ? DSLHeroName(hero, Players.GetLocalPlayer()) : "";
        if (hero > 0 && heroName) targets.push([hero, heroName]);
        // what the match's glows see, once per change (a regular match differs from a lobby)
        var mine = 0, hosted = 0;
        for (var gd = 0; gd < glowList.length; gd++) {
          if (glowList[gd].npc !== heroName) continue;
          mine++;
          if (hero > 0 && hostOf(glowList[gd], hero)) hosted++;
        }
        var diag = "hero " + hero + (heroValid ? "" : " (not valid)") + " " + heroName + ": " + mine + " glows, " + hosted + " on item models";
        if (diag !== glowDiag) { glowDiag = diag; $.Msg("DSL_LOG match glows: " + diag); }
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
          // the item's model on the menu's other models but not on this one: it wears
          // another item there (the wardrobe's preview of another weapon), none of its effects
          if (!host && inMenu && gl.item && itemHosts[gl.item] && itemHosts[gl.item].length) continue;
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
              // "^": on the hero the item's model hangs on (its file's "parent")
              var att = "" + cp[2], target = on;
              if (att.charAt(0) === "^") { att = att.substring(1); target = ent; }
              Particles.SetParticleControlEnt(p, cp[0], target, kind, att, [0, 0, 0], true);
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
    DSLLastRefresh = Date.now();
    // the item page (Dota's popup over the pages): its preview scene loaded again, so its
    // effects are made anew (a prismatic gem's colour: the weapon's glow is coloured when
    // it is born). The page is reached through the popup manager, not the pages below
    try {
      var details = root.FindChildTraverse("ItemDetails");
      if (details && details.visible) {
        var stack = [details], n = 0, reloaded = 0;
        while (stack.length && n < 6000) {
          var p = stack.pop();
          n++;
          if (!p.visible) continue;
          if (p.paneltype === "DOTAUIEconSetPreview") {
            var scene = p.FindChildTraverse("Preview3DItems");
            if (scene && typeof scene.ReloadScene === "function") { scene.ReloadScene(); reloaded++; }
            continue;
          }
          for (var c = 0; c < p.GetChildCount(); c++) stack.push(p.GetChild(c));
        }
        $.Msg("DSL_LOG refresh: item page previews reloaded: " + reloaded);
      }
    } catch (e) { $.Msg("DSL_LOG refresh: item page error " + e); }
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
      $.Schedule(tryOn ? 0.08 : 0, function () { $.DispatchEvent("DOTALoadoutChanged"); redrawn(); });
      // «REVERT» rebuilds the model asynchronously and may override the first refresh: repeat;
      // after a set try-on Dota brings back the old model from its cache later: once more after 2 s
      $.Schedule(0.18, function () { $.DispatchEvent("DOTALoadoutChanged"); redrawn(); });
      // (no read after 2 s for a set try-on's model Dota brought back from its cache: the
      // scene loaded anew below is made from the loadout; a read after it changed the hero
      // in place again)
      // then the hero's scene loaded anew: on the loadout's change the page changes its
      // hero's models in place, and a body changed so draws nothing of the hero but some
      // pieces (Razor's arcana Tormentor out for another whip: only his armor and bracers;
      // until then the arcana stayed). Once, after the last loadout read above (twice
      // flickered)
      var model = hp.FindChildTraverse("FocusedHeroSetPreview");
      var scene3d = model ? model.FindChildTraverse("Preview3DItems") : null;
      if (scene3d && typeof scene3d.ReloadScene === "function")
        $.Schedule(0.25, function () { if (scene3d.IsValid()) { scene3d.ReloadScene(); redrawn(); } });
      $.Msg("DSL_LOG refresh: loadout changed (revert " + (revert ? (shown ? "shown" : "hidden") : "none") + ", controls " + (controls ? (controls.visible ? "shown" : "hidden") : "none") + ")");
      return;
    }
    // no hero page: the other previews (the item page's) are rebuilt by the same event
    $.DispatchEvent("DOTALoadoutChanged");
    $.Msg("DSL_LOG refresh: no hero page open, loadout changed for the other previews (" + loopKind + " loop)");
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
                         "if (typeof DSLSetSlots === 'function') DSLSetSlots(" + g_slotsJs + ");"
                         "if (typeof DSLSetGems === 'function') DSLSetGems(" + g_gemColorsJs + "," + g_gemPicksJs + "," + g_gemItemsJs + ");"
                         "if (typeof DSLSetPreset === 'function') DSLSetPreset(" + g_presetJs + ");";
    ReleaseSRWLockShared(&g_glowsLock);
    // (a new script context of the menu: which heroes show their persona)
    AcquireSRWLockShared(&g_personaLock);
    script += "DSLPersonas = " + g_personaKeysJs + ";DSLDressed = " + g_dressedJs + ";";
    ReleaseSRWLockShared(&g_personaLock);
    Log("[*] Sending item glows to the game UI (%s)", kLoopNames[kind]);
    t_inOurScript = true;
    // a new name every time (and per loop): Panorama keeps a compiled script by its name,
    // and the same name ran the first table's script again (the slot icons stayed old)
    std::string name = std::string("dota_skins_table_") + kLoopNames[kind] + "_" + std::to_string(v) + ".js";
    g_OrigRunScript(g_uiEngine, panel, script.c_str(), name.c_str(), 0);
    t_inOurScript = false;
}

// Debugging: a script put in debug_run.js (the app folder) runs once in the hero page's
// context when the file changes (its $.Msg("DSL_LOG ...") lines go to the log)
static void MaybeRunDebugScript() {
    static FILETIME last{};
    static ULONGLONG checked = 0;
    if (GetTickCount64() - checked < 500) return;
    checked = GetTickCount64();
    void* panel = g_loopPanels[kDash];
    if (!g_uiEngine || !panel || !g_OrigRunScript) return;
    std::string path = g_appDir + "debug_run.js";
    WIN32_FILE_ATTRIBUTE_DATA data;
    if (!GetFileAttributesExA(path.c_str(), GetFileExInfoStandard, &data)) return;
    if (CompareFileTime(&data.ftLastWriteTime, &last) == 0) return;
    bool first = last.dwLowDateTime == 0 && last.dwHighDateTime == 0;
    last = data.ftLastWriteTime;
    if (first) return;  // (one left from before: not run at the start)
    FILE* f = fopen(path.c_str(), "rb");
    if (!f) return;
    std::string script;
    char buf[4096];
    size_t n;
    while ((n = fread(buf, 1, sizeof buf, f)) > 0) script.append(buf, n);
    fclose(f);
    static int runs = 0;
    std::string name = "dota_skins_debug_" + std::to_string(++runs) + ".js";
    Log("[*] Debug script %d (%zu bytes)", runs, script.size());
    t_inOurScript = true;
    g_OrigRunScript(g_uiEngine, panel, script.c_str(), name.c_str(), 0);
    t_inOurScript = false;
}

static void MaybeRefreshPreview() {
    MaybeRunDebugScript();
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

// --- Personas (see g_personaDefs) ---
// CDOTAPlayerInventory::GetItemInLoadout(inventory, hero id, slot index, ignore previews):
// the item a slot of the hero shows (the equipped one, else the slot's default). The
// inventory manager holds the local player's inventory at +0x120 and the heroes' loadout
// data: per hero 32 slots of 0x50 bytes from +8, the slot's type first (0x4E: the persona)
using GetItemInLoadoutFn = void* (*)(void* inventory, int hero, int slot, bool ignorePreview);
using HeroLoadoutDataFn = uint8_t* (*)(void* manager, int hero);
// CEconItemView: its constructor, and its Init(definition, quality, level, account,
// attributes, style), as the game makes the items a hero previews (0xd8 bytes)
using ItemViewCtorFn = void (*)(void* view);
using ItemViewInitFn = void (*)(void* view, int def, int quality, int level, uint32_t account, void* attributes, uint8_t style);
static GetItemInLoadoutFn g_OrigGetItemInLoadout = nullptr;
static HeroLoadoutDataFn g_HeroLoadoutData = nullptr;
static ItemViewCtorFn g_ItemViewCtor = nullptr;
static ItemViewInitFn g_ItemViewInit = nullptr;
static uint8_t* g_inventoryManager = nullptr;
static const int kInventoryOffset = 0x120;
static const int kPersonaSlotType = 0x4E;
// hero -> its slots' items (slot index, item view, whether it is the persona): made on the
// main thread, then only read
struct PersonaSlot { int slot; void* view; bool persona; };
struct PersonaTable { std::unordered_map<int, std::vector<PersonaSlot>> heroes; };
static PersonaTable* volatile g_personaTable = nullptr;

static void* HookedGetItemInLoadout(void* inventory, int hero, int slot, bool ignorePreview) {
    PersonaTable* t = g_personaTable;
    bool menu = g_inMenu != 0;
    if (t && inventory == g_inventoryManager + kInventoryOffset && (menu || hero == g_localHeroId)) {
        auto it = t->heroes.find(hero);
        if (it != t->heroes.end())
            for (auto& s : it->second)
                // in a match the persona only (the strategy screen's switch and preview): the
                // hero's items there are the files' (a dressed hero's too)
                if (s.slot == slot && (menu || s.persona)) return s.view;
    }
    return g_OrigGetItemInLoadout(inventory, hero, slot, ignorePreview);
}

// The index of the hero's slot of a loadout type, -1 if it has none
static int SlotIndexOf(int hero, int type) {
    uint8_t* data = g_HeroLoadoutData && type >= 0 ? g_HeroLoadoutData(g_inventoryManager, hero) : nullptr;
    if (!data) return -1;
    for (int i = 0; i < 32; i++)
        if (*(const int*)(data + 8 + i * 0x50) == type) return i;
    return -1;
}

// A loadout slot's type by its name ("hero_base"): the game's enum names (its table of
// "DOTA_LOADOUT_TYPE_HERO_BASE" and the like, each with its value 8 bytes on)
struct Section;
static uint8_t* FindString(const Section& rdata, const char* s, bool prefix);
static const Section* g_clientRdata = nullptr;
static const Section* g_clientData = nullptr;
static int SlotTypeOfIn(const std::string& name);
static int SlotTypeOf(const std::string& slot) {
    static std::map<std::string, int> cache;  // main thread
    auto c = cache.find(slot);
    if (c != cache.end()) return c->second;
    std::string name = "DOTA_LOADOUT_TYPE_";
    for (char ch : slot) name += (char)toupper((unsigned char)ch);
    int type = SlotTypeOfIn(name);
    cache[slot] = type;
    return type;
}

// One item view per item and style, kept for good (main thread)
static void* ItemView(int def, int style = 0) {
    static std::map<std::pair<int, int>, void*> views;
    auto it = views.find({def, style});
    if (it != views.end()) return it->second;
    void* view = nullptr;
    if (g_ItemViewCtor && g_ItemViewInit) {
        view = _aligned_malloc(0x200, 16);
        if (view) {
            memset(view, 0, 0x200);
            g_ItemViewCtor(view);
            // quality and level "any" (9999), as the game's previews
            g_ItemViewInit(view, def, 9999, 9999, 0, nullptr, (uint8_t)style);
        }
    }
    views[{def, style}] = view;
    return view;
}

// Main thread, the menu loop's tick (in a match the HUD loop's): the table again when the
// personas changed, and the hero page or the strategy screen (its switch, model and slots)
// told to read the loadout again
static void PreloadPersonaModels();

static void PersonaTick(int kind) {
    static LONG built = 0;
    LONG v = g_personaVersion;
    if (!g_OrigGetItemInLoadout || v == built) return;
    void* panel = g_loopPanels[kind];
    if (!g_uiEngine || !panel || !g_OrigRunScript) return;
    built = v;
    // a match: the persona's models ready before the strategy screen's preview is made again
    // (it gets them by name then)
    if (!g_inMenu) PreloadPersonaModels();
    std::vector<PersonaDef> wanted;
    std::string keys;  // for the script: {HERO: 1}
    AcquireSRWLockShared(&g_personaLock);
    for (auto& d : g_personaDefs) {
        auto p = g_personaPicked.find(d.second.hero);
        bool on = p != g_personaPicked.end() ? p->second.on : g_personaOn.count(d.second.hero) > 0;
        if (!on) continue;
        wanted.push_back(d.second);
        keys += (keys.empty() ? "" : ",") + JsString(d.first) + ":1";
    }
    ReleaseSRWLockShared(&g_personaLock);
    std::vector<WearItem> wear;
    AcquireSRWLockShared(&g_personaLock);
    wear = g_wearItems;
    ReleaseSRWLockShared(&g_personaLock);
    auto* table = new PersonaTable();
    std::set<int> personaHeroes;
    for (auto& d : wanted) {
        int slot = SlotIndexOf(d.hero, kPersonaSlotType);
        void* view = slot >= 0 ? ItemView(d.def) : nullptr;
        if (view) table->heroes[d.hero].push_back({slot, view, true});
        personaHeroes.insert(d.hero);
        Log("[%c] Persona: hero %d wears persona item %d (slot %d, item %p)", view ? '+' : '-', d.hero, d.def, slot, view);
    }
    if (wanted.empty()) Log("[*] Persona: none worn");
    // the dressed heroes' items (not over a persona shown: those are its classic look's)
    std::string dressed;
    std::set<std::string> dressedNpcs, ownGlows;
    for (auto& w : wear)
        if (w.ownGlows) ownGlows.insert(w.npc);
    for (auto& w : wear) {
        if (personaHeroes.count(w.hero)) continue;
        int type = SlotTypeOf(w.slot);
        int slot = SlotIndexOf(w.hero, type);
        void* view = slot >= 0 ? ItemView(w.def, w.style) : nullptr;
        if (view) table->heroes[w.hero].push_back({slot, view, false});
        Log("[%c] Worn: hero %d %s item %d style %d (type %d, slot %d)", view ? '+' : '-', w.hero, w.slot.c_str(), w.def, w.style, type, slot);
        // (the script's glows stay on a hero whose items' effects are ours)
        if (view && dressedNpcs.insert(w.npc).second && !ownGlows.count(w.npc))
            dressed += (dressed.empty() ? "" : ",") + JsString(w.npc) + ":1";
    }
    // the old table is left alone: another thread may be reading it (a few bytes per change)
    InterlockedExchangePointer((void* volatile*)&g_personaTable, table);
    static int runs = 0;
    std::string name = "dota_skins_persona_" + std::to_string(++runs) + ".js";
    AcquireSRWLockExclusive(&g_personaLock);
    g_personaKeysJs = "{" + keys + "}";
    g_dressedJs = "{" + dressed + "}";
    g_dressedNpcs = dressedNpcs;
    ReleaseSRWLockExclusive(&g_personaLock);
    std::string script = "DSLPersonas = {" + keys + "};"
                         "DSLDressed = {" + dressed + "};"
                         "if (typeof DSLClearGlows === 'function') DSLClearGlows();"
                         "$.DispatchEvent('DOTALoadoutChanged');"
                         "if (typeof DSLSlotForce === 'function') DSLSlotForce();";
    t_inOurScript = true;
    g_OrigRunScript(g_uiEngine, panel, script.c_str(), name.c_str(), 0);
    t_inOurScript = false;
}

// A hero name as the script and the launcher compare it: upper case letters and digits
static std::string HeroKey(const std::string& name) {
    std::string out;
    for (unsigned char c : name) {
        if (c >= 0x80) out += (char)c;
        else if (isalnum(c)) out += (char)toupper(c);
    }
    return out;
}

// The page's switch pressed (pick 0|@persona<TAB>hero<TAB>on|off): shown at once, before
// the launcher puts the persona into the preset (or takes it out)
static void PersonaPicked(const char* payload) {
    const char* bar = payload ? strchr(payload, '|') : nullptr;
    if (!bar || strncmp(bar + 1, "@persona\t", 9) != 0) return;
    auto f = Split(bar + 10, '\t');
    if (f.size() < 2) return;
    // (the message ends with a line break)
    for (auto& s : f) {
        while (!s.empty() && (s.back() == ' ' || s.back() == '\n' || s.back() == '\r')) s.pop_back();
    }
    bool on = f[1] == "on";
    bool known = false;
    AcquireSRWLockExclusive(&g_personaLock);
    auto d = g_personaDefs.find(HeroKey(f[0]));
    if (d != g_personaDefs.end()) {
        g_personaPicked[d->second.hero] = {on, GetTickCount64()};
        known = true;
    }
    ReleaseSRWLockExclusive(&g_personaLock);
    Log("[*] Persona %s for %s%s", on ? "on" : "off", f[0].c_str(), known ? "" : " (no persona known for it)");
    if (known) {
        UpdatePersonaShown();
        InterlockedIncrement(&g_personaVersion);
    }
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
        ? "DSLTxt = { select: \"SELECT\", selected: \"SELECTED\", have: \"OWNED\", restart: \"AFTER RESTART\", gemOwn: \"The item's own gem\", gemNote: \"Only you see the gem: the item on your account does not change.\" };"
        : "DSLTxt = { select: \"ВЫБРАТЬ\", selected: \"ВЫБРАНО\", have: \"ЕСТЬ У ВАС\", restart: \"ПОСЛЕ ПЕРЕЗАПУСКА\", gemOwn: \"Свой самоцвет предмета\", gemNote: \"Самоцвет видите только вы: предмет на аккаунте не меняется.\" };";
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
static void ResolveParticles(const Section& text, const Section& rdata);
static void ResolveProjectiles(const Section& text, const Section& rdata);
static void ResolveUiState(const Section& text, const Section& rdata);
static void ResolveWeather(HMODULE cl, const Section& text, const Section& rdata);
static void ResolveTerrainAndHud(const Section& text);
static void ResolveShaders(const Section& text);
static void ResolveMatGroups(const Section& text, const Section& rdata);
static void ResolveGems(const Section& text, const Section& rdata, const Section& data);
// CSkeletonInstance::SetModel(skeleton, model binding): see LiveModelTick
using SkeletonSetModelFn = void (*)(void* skeleton, void* binding);
static SkeletonSetModelFn g_SkeletonSetModel = nullptr;

static uint8_t* FindMasked(const Section& text, const char* sig) {
    std::vector<int> bytes;
    for (const char* c = sig; *c;) {
        if (*c == ' ') { c++; continue; }
        if (c[0] == '?') { bytes.push_back(-1); c += c[1] == '?' ? 2 : 1; continue; }
        bytes.push_back((int)strtoul(std::string(c, 2).c_str(), nullptr, 16));
        c += 2;
    }
    uint8_t* found = nullptr;
    for (size_t i = 0; i + bytes.size() <= text.size; i++) {
        size_t k = 0;
        while (k < bytes.size() && (bytes[k] < 0 || text.start[i + k] == bytes[k])) k++;
        if (k < bytes.size()) continue;
        if (found) return nullptr;  // not unique: do not risk it
        found = text.start + i;
    }
    return found;
}

// (see SlotTypeOf)
static int SlotTypeOfIn(const std::string& name) {
    const uint8_t* s = g_clientRdata ? FindString(*g_clientRdata, name.c_str(), false) : nullptr;
    if (!s) return -1;
    for (const Section* sec : {g_clientRdata, g_clientData}) {
        for (size_t i = 0; sec && i + 16 <= sec->size; i += 8)
            if (*(const uint8_t* const*)(sec->start + i) == s) return *(const int*)(sec->start + i + 8);
    }
    return -1;
}

// --- The stance of the preset's items (see ActMod) ---
// A hero (C_DOTA_BaseNPC) gathers its activity modifiers again and again: its items', its
// buffs'; a hero on a menu page or the strategy screen (C_PortraitWorldUnit) adds "loadout"
// and its own after them. Hooked: the preset's join them, so the game chooses the sequences
// they give: in the menu for every hero, in a match for the hero you play (its strategy
// screen preview, the hero itself, its illusions).
using NpcModsFn = void* (*)(void* npc, intptr_t activity, intptr_t b, intptr_t c);
using AddActModFn = uint16_t* (*)(void* npc, uint16_t* out, const char* name);
static NpcModsFn g_OrigNpcMods = nullptr;
static AddActModFn g_AddActMod = nullptr;
static int g_unitNameOffset = -1;  // C_DOTA_BaseNPC::m_iszUnitName (a string)
// C_DOTA_BaseNPC::m_nIsOriginalModel: 0 work it out (the model it was made with: its pieces
// shown), 1 yes, 2 no (pieces hidden, as for a transformation): a persona's body put on live
// is said to be one, or the game hid its pieces
static int g_isOriginalOffset = -1;
// C_DOTAWearableItem::m_bHiddenByCombiner and its setter: a piece merged into its hero's
// combined model is hidden; on a body put on live its pieces are shown again
static int g_hiddenByCombinerOffset = -1;
using SetHiddenByCombinerFn = void (*)(void* wearable, bool hidden);
static SetHiddenByCombinerFn g_SetHiddenByCombiner = nullptr;
static int EntityIndexOf(const uint8_t* ent);

// A unit name's pointer as an NPC holds it: nullptr unless it can be a string's (a
// client-side NPC of the interface leaves it empty)
static const char* UnitNameOf(const void* npc) {
    const char* unit = *(const char* const*)((const uint8_t*)npc + g_unitNameOffset);
    uintptr_t v = (uintptr_t)unit;
    return v > 0x10000 && v < 0x7FFFFFFFFFFFull ? unit : nullptr;
}

// The hero (npc name) whose preset modifiers an NPC takes: in the menu any hero, in a match
// the hero you play (a regular match gives its units no names: your hero by its entity
// then); empty for none
static std::string StanceHeroOf(void* npc) {
    if (!g_actModsCount || g_unitNameOffset <= 0) return std::string();
    const char* unit = UnitNameOf(npc);
    if (g_inMenu) return unit && strncmp(unit, "npc_dota_hero_", 14) == 0 ? std::string(unit) : std::string();
    std::string who;
    AcquireSRWLockShared(&g_rulesLock);
    if (!g_localNpc.empty()) {
        if (unit && g_localNpc == unit) who = g_localNpc;
        else if ((!unit || !*unit) && g_heroIndex > 0 && g_itemHostsReady && EntityIndexOf((const uint8_t*)npc) == g_heroIndex)
            who = g_localNpc;
    }
    ReleaseSRWLockShared(&g_rulesLock);
    return who;
}

// The preset's modifiers of a hero (its persona's while that is shown, else its classic
// look's), up to `max`
static int StanceMods(const std::string& who, std::string* out, int max) {
    int n = 0;
    AcquireSRWLockShared(&g_personaLock);
    for (auto& m : g_actMods)
        if (n < max && m.npc == who && m.persona == PersonaShownLocked(m.hero)) out[n++] = m.mod;
    ReleaseSRWLockShared(&g_personaLock);
    return n;
}

static void* HookedNpcMods(void* npc, intptr_t activity, intptr_t b, intptr_t c) {
    void* r = g_OrigNpcMods(npc, activity, b, c);
    if (!g_actModsCount || !g_AddActMod) return r;
    std::string who = StanceHeroOf(npc);
    if (who.empty()) return r;
    std::string mods[16];
    int n = StanceMods(who, mods, 16);
    for (int i = 0; i < n; i++) {
        uint16_t sym = 0xFFFF;
        g_AddActMod(npc, &sym, mods[i].c_str());  // (one it has already is not added twice)
    }
    return r;
}

// --- The stance of the preset's items on the match's hero ---
// The server chooses a networked NPC's sequences (your hero's without the preset's items)
// and sends the activity and the sequence's place among that activity's sequences; on the
// change the client plays the one at that place in its model (OnDataChanged). For the hero
// you play the client chooses again, with the preset's modifiers joined to its own, and
// plays that one (Legion Commander's arcana: both swords in hand).
using NetSeqFn = void* (*)(void* npc, intptr_t a, intptr_t b, intptr_t c);
using SetSeqFn = void* (*)(void* npc, intptr_t seq, intptr_t a, intptr_t b);
static NetSeqFn g_OrigNetSeq = nullptr;
static SetSeqFn g_SetSequence = nullptr;
static uint32_t g_netActivityOffset = 0;  // C_DOTA_BaseNPC::m_NetworkActivity

static void* HookedNetSeq(void* npc, intptr_t a, intptr_t b, intptr_t c) {
    void* r = g_OrigNetSeq(npc, a, b, c);
    if (g_inMenu || !g_actModsCount || !g_SetSequence || !g_netActivityOffset || !g_selectSlot || !g_AddSymbol ||
        !g_modSymbols || !g_modsCount || !g_modsData)
        return r;
    std::string who = StanceHeroOf(npc);
    if (who.empty()) return r;
    std::string names[16];
    int n = StanceMods(who, names, 16);
    if (!n) return r;
    int activity = *(const int*)((const uint8_t*)npc + g_netActivityOffset);
    if (activity < 0) return r;
    std::vector<uint16_t> mods;
    int cnt = *(const int*)((const uint8_t*)npc + g_modsCount);
    const uint16_t* data = *(uint16_t* const*)((const uint8_t*)npc + g_modsData);
    for (int k = 0; data && k < cnt && k < 32; k++) mods.push_back(data[k]);
    for (int i = 0; i < n; i++) {
        uint16_t sym = 0xFFFF;
        g_AddSymbol(g_modSymbols, &sym, names[i].c_str(), nullptr);
        if (sym != 0xFFFF && std::find(mods.begin(), mods.end(), sym) == mods.end()) mods.push_back(sym);
    }
    auto select = (SelectSeqFn)(*(void***)npc)[g_selectSlot / 8];
    int seq = -1;
    select(npc, &seq, activity, mods.data(), (int)mods.size());
    if (seq < 0) return r;
    g_SetSequence(npc, seq, 2, 1);
    static std::set<std::pair<int, int>> logged;
    if (logged.size() < 64 && logged.insert({activity, seq}).second)
        Log("[*] Stance: %s activity %d plays sequence %d (with the preset's modifiers)", who.c_str(), activity, seq);
    return r;
}

static void ResolveStances(const Section& text, const Section& rdata, const Section& data) {
    SchemaField want[] = {{"C_DOTA_BaseNPC", "m_iszUnitName", &g_unitNameOffset},
                          {"C_DOTA_BaseNPC", "m_nIsOriginalModel", &g_isOriginalOffset},
                          {"C_DOTAWearableItem", "m_bHiddenByCombiner", &g_hiddenByCombinerOffset}};
    ReadSchemaOffsets(rdata, data, want, 3);
    // push rbx; sub rsp,20h; mov rbx,rcx; cmp [rcx+X],dl; je; mov [rcx+X],dl; xor edx,edx; call
    g_SetHiddenByCombiner = (SetHiddenByCombinerFn)FindMasked(text,
        "40 53 48 83 EC 20 48 8B D9 38 91 ?? ?? 00 00 74 ?? 88 91 ?? ?? 00 00 33 D2 E8");
    // mov [rsp+20h],rsi; push rbp; push rdi; push r15; mov rbp,rsp; sub rsp,60h;
    // mov [rsp+90h],r14; mov rdi,rcx; call <the NPC's>; cmp byte [rdi+X],0; jne;
    // lea r8,"loadout"; mov rcx,rdi; lea rdx,[rbp+20h]; call <add activity modifier>
    uint8_t* fn = FindMasked(text,
        "48 89 74 24 20 55 57 41 57 48 8B EC 48 83 EC 60 4C 89 B4 24 90 00 00 00 48 8B F9 E8 ?? ?? ?? ?? "
        "80 BF ?? ?? 00 00 00 75 ?? 4C 8D 05 ?? ?? ?? ?? 48 8B CF 48 8D 55 20 E8");
    if (fn) g_AddActMod = (AddActModFn)(fn + 60 + *(int32_t*)(fn + 56));
    // the NPC's own, which it calls first: mov rax,rsp; push rbp/rbx/rsi/rdi; lea rbp,[rax-X];
    // sub rsp,X (the first 14 bytes, to the lea, have no RIP-relative instructions)
    uint8_t* npcMods = fn ? fn + 32 + *(int32_t*)(fn + 28) : nullptr;
    static const uint8_t kNpcProlog[] = {0x48, 0x8B, 0xC4, 0x55, 0x53, 0x56, 0x57, 0x48, 0x8D, 0xA8};
    if (npcMods && (memcmp(npcMods, kNpcProlog, sizeof(kNpcProlog)) != 0 || npcMods[14] != 0x48 ||
                    npcMods[15] != 0x81 || npcMods[16] != 0xEC))
        npcMods = nullptr;
    bool ok = npcMods && g_AddActMod && g_unitNameOffset > 0 &&
              InstallHook(npcMods, 14, (void*)&HookedNpcMods, (void**)&g_OrigNpcMods);
    Log("[%c] Stances: NPC modifiers %p (portrait's %p), add modifier %p, unit name +0x%X, original model +0x%X, "
        "hidden by combiner +0x%X (%p)", ok ? '+' : '-', (void*)npcMods, (void*)fn, (void*)g_AddActMod, g_unitNameOffset,
        g_isOriginalOffset, g_hiddenByCombinerOffset, (void*)g_SetHiddenByCombiner);
    // the match's hero: its sequence choice (the taunts' finds: modifiers, symbols, choice)
    if (!g_tauntResolved) ResolveTaunts();
    // mov rax,rsp; push r15; sub rsp,90h; movaps [rax-48h],xmm7; mov r15,rcx; mov [rax+18h],rbx;
    // mov ebx,[rcx+<m_NetworkActivity>]; ... (the first 16 bytes, to the movaps, have no
    // RIP-relative instructions); in it: mov r9d,1; mov r8d,2; mov rcx,r15; call <SetSequence>
    uint8_t* net = FindMasked(text,
        "48 8B C4 41 57 48 81 EC 90 00 00 00 0F 29 78 B8 4C 8B F9 48 89 58 18 8B 99 ?? ?? 00 00 48 89 78 E8 "
        "4C 89 60 E0 44 0F B6 E2 E8");
    static const uint8_t kSetSeq[] = {0x41, 0xB9, 0x01, 0x00, 0x00, 0x00, 0x41, 0xB8, 0x02, 0x00, 0x00, 0x00, 0x49, 0x8B, 0xCF, 0xE8};
    g_SetSequence = (SetSeqFn)CallAfter(net, 0x200, kSetSeq, sizeof(kSetSeq));
    if (net) g_netActivityOffset = *(uint32_t*)(net + 25);
    bool netOk = net && g_SetSequence && g_netActivityOffset && g_selectSlot && g_modsCount && g_modsData &&
                 g_AddSymbol && g_modSymbols && InstallHook(net, 16, (void*)&HookedNetSeq, (void**)&g_OrigNetSeq);
    Log("[%c] Stances in a match: network sequence %p, SetSequence %p, activity +0x%X, choice +0x%X", netOk ? '+' : '-',
        (void*)net, (void*)g_SetSequence, g_netActivityOffset, g_selectSlot);
}

static void ResolvePersonas(const Section& text, const Section& rdata, const Section& data) {
    static Section keptRdata, keptData;
    keptRdata = rdata;
    keptData = data;
    g_clientRdata = &keptRdata;
    g_clientData = &keptData;
    // mov [rsp+18h],rbp; mov [rsp+10h],edx; push rsi; push rdi; push r14; sub rsp,20h;
    // movsxd rsi,r8d; movzx ebp,r9b; mov rdi,rcx; cmp esi,1Fh; ja; lea r14,[manager];
    // mov rcx,r14; call <hero loadout data>; ...
    uint8_t* get = FindMasked(text,
        "48 89 6C 24 18 89 54 24 10 56 57 41 56 48 83 EC 20 49 63 F0 41 0F B6 E9 48 8B F9 83 FE 1F "
        "0F 87 ?? ?? ?? ?? 4C 8D 35 ?? ?? ?? ?? 49 8B CE E8 ?? ?? ?? ?? 48 85 C0 0F 84 ?? ?? ?? ?? "
        "48 89 5C 24 40 8B 5C 24 48 40 84 ED");
    g_ItemViewCtor = (ItemViewCtorFn)FindMasked(text,
        "48 89 5C 24 08 48 89 74 24 10 57 48 83 EC 20 33 F6 48 8D 05 ?? ?? ?? ?? 48 89 71 08 48 8B F9 "
        "89 71 10 48 89 01 48 89 71 18 48 89 71 20 66 89 71 30 48 89 71 48 48 89 71 50 48 83 C1 58 E8");
    g_ItemViewInit = (ItemViewInitFn)FindMasked(text,
        "40 53 48 83 EC 40 0F B6 84 24 80 00 00 00 48 8B D9 88 44 24 30 48 8B 44 24 78 48 89 44 24 28 "
        "8B 44 24 70 89 44 24 20 E8 ?? ?? ?? ?? 48 8B 8B D0 00 00 00");
    if (get) {
        g_inventoryManager = get + 0x2B + *(int32_t*)(get + 0x27);
        g_HeroLoadoutData = (HeroLoadoutDataFn)(get + 0x33 + *(int32_t*)(get + 0x2F));
    }
    // the first 17 bytes (to sub rsp) have no RIP-relative instructions
    bool ok = get && g_ItemViewCtor && g_ItemViewInit &&
              InstallHook(get, 17, (void*)&HookedGetItemInLoadout, (void**)&g_OrigGetItemInLoadout);
    if (!ok) g_OrigGetItemInLoadout = nullptr;
    Log("[%c] Personas: GetItemInLoadout %p, inventory manager %p, hero loadout data %p, item view %p / %p",
        ok ? '+' : '-', (void*)get, (void*)g_inventoryManager, (void*)g_HeroLoadoutData, (void*)g_ItemViewCtor,
        (void*)g_ItemViewInit);
}

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
    ResolveParticles(text, rdata);
    ResolveProjectiles(text, rdata);
    ResolveUiState(text, rdata);
    ResolveWeather(cl, text, rdata);
    ResolveTerrainAndHud(text);
    ResolveShaders(text);
    ResolveMatGroups(text, rdata);
    ResolveGems(text, rdata, data);
    ResolvePersonas(text, rdata, data);
    ResolveStances(text, rdata, data);
    // the function asserting "CSkeletonInstance::SetModel"
    g_SkeletonSetModel = (SkeletonSetModelFn)FunctionStart(FindRipRef(text, FindString(rdata, "CSkeletonInstance::SetModel")));
    Log("[%c] Live models: CSkeletonInstance::SetModel %p", g_SkeletonSetModel ? '+' : '-', (void*)g_SkeletonSetModel);
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

// The item models of the glows found by the last scan (their entities: valid during the
// tick that scanned only)
struct ItemHost { std::string key; int index; uint32_t handle; int parent, owner; uint8_t* ent; };
static std::vector<ItemHost> g_hosts;

// The item models of the glows in the world, as the script's table:
// {"item model":[[entity,handle,parent,owner],…],…}
static std::string ScanItemHosts() {
    g_hosts.clear();
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
            g_hosts.push_back({it->second, index, handle, parent, owner, (uint8_t*)ent});
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
static std::string g_itemHostsJs = "{}";
// a scan at most every 200 ms (both loops tick)
static void RefreshItemHosts() {
    if (!g_itemHostsReady || !g_entityList) return;
    static ULONGLONG lastScan = 0;
    ULONGLONG now = GetTickCount64();
    if (now - lastScan < 200) return;
    lastScan = now;
    g_itemHostsJs = ScanItemHosts();
}
static void MaybeSendItemHosts(int kind) {
    void* panel = g_loopPanels[kind];
    if (!g_itemHostsReady || !g_entityList || !g_uiEngine || !panel || !g_OrigRunScript) return;
    RefreshItemHosts();
    const std::string& js = g_itemHostsJs;
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

// --- The glows in a match, created by the DLL itself ---
// A regular match gives the interface scripts no entities: Panorama's Particles and
// Entities bindings look an entity up only in the menu, a demo or a custom game (the
// game mode check in client.dll), so the script's glows hung on nothing there. The DLL
// calls what those bindings call, with the item's own model (found in the entity list)
// as the effect's owner and its control points' entity: the item's own effect on your
// own hero, as the game would show it.
struct ParticleDesc {                // the binding's description of a new effect (0x70 bytes)
    const char* name;                // 0x00
    int32_t attach, pad0;            // 0x08
    void* entity;                    // 0x10: the owner
    uint64_t z18, z20;               // 0x18
    int32_t z28, pad1;               // 0x28
    uint64_t z30;                    // 0x30
    uint16_t z38; uint8_t z3a, pad2; // 0x38
    float m3c, m40;                  // 0x3c: -1
    float f44, f48, f4c;             // 0x44: FLT_MAX
    int32_t z50, pad3;               // 0x50
    uint64_t z58;                    // 0x58
    int32_t m60;                     // 0x60: -1
    float f64, f68, f6c;             // 0x64: FLT_MAX
};
static_assert(sizeof(ParticleDesc) == 0x70, "ParticleDesc layout");

using GetParticleMgrFn = void* (*)();
using CreateParticleFn = void (*)(void* mgr, int32_t* out, ParticleDesc* desc);
using SetControlFn = bool (*)(void* mgr, int32_t particle, int32_t cp, const float* vec, float unused);
using SetControlEntFn = bool (*)(void* mgr, int32_t particle, int32_t cp, void* ent, int32_t attach, const char* attachment,
                                 const float* offset, bool lockOrientation, void* zero);
using DestroyParticleFn = void (*)(void* mgr, int32_t particle, bool immediate, bool flag);
static GetParticleMgrFn g_ParticleMgr = nullptr;
static CreateParticleFn g_CreateParticle = nullptr;
static SetControlFn g_SetControl = nullptr;
static SetControlEntFn g_SetControlEnt = nullptr;
static DestroyParticleFn g_DestroyParticle = nullptr;
static volatile LONG g_particlesReady = 0;

// A Panorama binding's function: the binding table names it right after loading it
// (lea rax,[function] ... lea rcx,["Name"]), in .text
static uint8_t* BindingImpl(const Section& text, const Section& rdata, const char* name) {
    const uint8_t* s = FindString(rdata, name);
    for (size_t i = 0; s && i + 7 <= text.size; i++) {
        uint8_t* p = text.start + i;
        if (!((p[0] == 0x48 || p[0] == 0x4C) && p[1] == 0x8D && (p[2] & 0xC7) == 0x05 && p + 7 + *(int32_t*)(p + 3) == s)) continue;
        for (uint8_t* q = p - 7; q >= p - 0x28; q--) {
            if (q[0] != 0x48 || q[1] != 0x8D || q[2] != 0x05) continue;
            uint8_t* impl = q + 7 + *(int32_t*)(q + 3);
            if (impl >= text.start && impl < text.start + text.size) return impl;
            break;
        }
    }
    return nullptr;
}

// The target of the call or jump that ends `pattern` within `len` bytes of `from`
static uint8_t* CallIn(uint8_t* from, size_t len, const uint8_t* pattern, size_t n) {
    return from ? CallAfter(from, len, pattern, n) : nullptr;
}

static void ResolveParticles(const Section& text, const Section& rdata) {
    uint8_t* create = BindingImpl(text, rdata, "CreateParticle");
    uint8_t* setCp = BindingImpl(text, rdata, "SetParticleControl");
    uint8_t* setCpEnt = BindingImpl(text, rdata, "SetParticleControlEnt");
    uint8_t* destroy = BindingImpl(text, rdata, "DestroyParticleEffect");
    uint8_t* release = BindingImpl(text, rdata, "ReleaseParticleIndex");
    // push rbx; sub rsp,20h; mov ebx,edx; call <manager>
    static const uint8_t kMgr[] = {0x40, 0x53, 0x48, 0x83, 0xEC, 0x20, 0x8B, 0xDA, 0xE8};
    g_ParticleMgr = (GetParticleMgrFn)CallIn(release, 1, kMgr, sizeof(kMgr));
    // mov dword [rsp+80h],-1; call <create>
    static const uint8_t kCreate[] = {0xC7, 0x84, 0x24, 0x80, 0x00, 0x00, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xE8};
    g_CreateParticle = (CreateParticleFn)CallIn(create, 0x140, kCreate, sizeof(kCreate));
    // movss [rsp+20h],xmm0; mov edx,esi; mov rcx,rax; call <set>
    static const uint8_t kSet[] = {0xF3, 0x0F, 0x11, 0x44, 0x24, 0x20, 0x8B, 0xD6, 0x48, 0x8B, 0xC8, 0xE8};
    g_SetControl = (SetControlFn)CallIn(setCp, 0x80, kSet, sizeof(kSet));
    // mov rcx,rax; movsd [rsp+60h],xmm0; call <set ent>
    static const uint8_t kSetEnt[] = {0x48, 0x8B, 0xC8, 0xF2, 0x0F, 0x11, 0x44, 0x24, 0x60, 0xE8};
    g_SetControlEnt = (SetControlEntFn)CallIn(setCpEnt, 0x140, kSetEnt, sizeof(kSetEnt));
    // add rsp,20h; pop rdi; jmp <destroy>
    static const uint8_t kDestroy[] = {0x48, 0x83, 0xC4, 0x20, 0x5F, 0xE9};
    g_DestroyParticle = (DestroyParticleFn)CallIn(destroy, 0x40, kDestroy, sizeof(kDestroy));
    bool ok = g_ParticleMgr && g_CreateParticle && g_SetControl && g_SetControlEnt && g_DestroyParticle;
    Log("[%c] Match glows: manager %p, create %p, control %p, control ent %p, destroy %p", ok ? '+' : '-',
        (void*)g_ParticleMgr, (void*)g_CreateParticle, (void*)g_SetControl, (void*)g_SetControlEnt, (void*)g_DestroyParticle);
    if (ok) InterlockedExchange(&g_particlesReady, 1);
}

// --- The emblem's attack effect ---
// An owned emblem's attack effect (Diretide, TI10, the 2021/2022 events) is not a file over
// the hero's projectile: the server names it in the projectile's message
// (CDOTAUserMsg_TE_Projectile.additional_particle_system_handle) and the client creates it
// as a child of the projectile's own effect, which gives it its points (from the attacker
// to the target, the speed). Copied over a hero's projectile file it showed nothing.
// C_DOTA_ProjectileManager::CreateTrackingProjectile(manager, info) gets the message's
// fields resolved: in this build info+0x08 the projectile's effect, +0x10 the additional
// one, +0x20 the attacker's handle, +0x35 whether it is an attack (checked against the
// function's code before it is hooked). An attack of the hero you play gets the preset
// emblem's effect there.
using CreateTrackingFn = int32_t (*)(void* manager, uint8_t* info);
static CreateTrackingFn g_OrigCreateTracking = nullptr;
static bool g_trackingHooked = false;
static void* volatile g_attackBinding = nullptr;  // the attack effect's resource, loaded
static std::string g_attackBound;                 // which effect it is (main thread)
static ULONGLONG g_attackChecked = 0;             // when it was last seen loaded (main thread)
static volatile LONG g_attacksMarked = 0;

static int32_t HookedCreateTracking(void* manager, uint8_t* info) {
    void* fx = g_attackBinding;
    if (fx && info && info[0x35] && g_heroIndex > 0) {
        uint32_t source = *(uint32_t*)(info + 0x20);
        if (source != 0xFFFFFFFF && (int)(source & 0x7FFF) == g_heroIndex) {
            *(void**)(info + 0x10) = fx;
            if (InterlockedIncrement(&g_attacksMarked) == 1) Log("[*] Emblem: its attack effect is on your attacks");
        }
    }
    return g_OrigCreateTracking(manager, info);
}

static bool HasBytes(const uint8_t* from, size_t len, const uint8_t* pattern, size_t n) {
    for (const uint8_t* p = from; from && p + n <= from + len; p++)
        if (memcmp(p, pattern, n) == 0) return true;
    return false;
}

static void ResolveProjectiles(const Section& text, const Section& rdata) {
    uint8_t* fn = FunctionStart(FindRipRef(text, FindString(rdata,
        "CL:  C_DOTA_ProjectileManager::CreateTrackingProjectile - pSource is NULL without an override loc (handle entity %d)?\n")));
    // mov [rsp+8],rcx; push rbp; push rbx; push r12; push r13; push r14; push r15 (15 bytes, no RIP-relative)
    static const uint8_t kProlog[] = {0x48, 0x89, 0x4C, 0x24, 0x08, 0x55, 0x53, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41, 0x57};
    // the info's fields as this build reads them: mov r12,rdx; mov edx,[rdx+20h] (the attacker)
    static const uint8_t kSource[] = {0x4C, 0x8B, 0xE2, 0x8B, 0x52, 0x20};
    // movzx eax,byte [r12+35h]; mov [r15+35h],al (an attack, into the projectile)
    static const uint8_t kAttack[] = {0x41, 0x0F, 0xB6, 0x44, 0x24, 0x35, 0x41, 0x88, 0x47, 0x35};
    // mov rdx,[r12+10h]; lea rcx,[rbp-70h]; mov r8,r13; call (the additional effect, as the
    // projectile's own at +0x08 a moment before)
    static const uint8_t kExtra[] = {0x49, 0x8B, 0x54, 0x24, 0x10, 0x48, 0x8D, 0x4D, 0x90, 0x4D, 0x8B, 0xC5, 0xE8};
    size_t span = fn ? (size_t)std::min<ptrdiff_t>(0x1000, text.start + text.size - fn) : 0;
    bool layout = fn && span > sizeof(kProlog) && memcmp(fn, kProlog, sizeof(kProlog)) == 0 &&
                  HasBytes(fn, 0x100, kSource, sizeof(kSource)) && HasBytes(fn, span, kAttack, sizeof(kAttack)) &&
                  HasBytes(fn, span, kExtra, sizeof(kExtra));
    if (layout && InstallHook(fn, sizeof(kProlog), (void*)&HookedCreateTracking, (void**)&g_OrigCreateTracking))
        g_trackingHooked = true;
    Log("[%c] Emblem attacks: CreateTrackingProjectile %p%s", g_trackingHooked ? '+' : '-', (void*)fn,
        g_trackingHooked ? " hooked" : layout ? " NOT hooked" : fn ? " (another layout: not hooked)" : "");
}

// --- The game's interface state ---
// CDOTA...::ChangeGameUIState(self, state) logs "ChangeGameUIState: %s -> %s" and stores the
// state: the loading screen comes before anything of the match is registered.
using ChangeUiStateFn = uint64_t (*)(void* self, int state);
static ChangeUiStateFn g_OrigChangeUiState = nullptr;

static uint64_t HookedChangeUiState(void* self, int state) {
    static const char* const kNames[] = {"none", "loading screen", "game", "dashboard"};
    LONG old = InterlockedExchange(&g_uiState, state);
    if (old != state)
        Log("[*] Interface: %s%s", state >= 0 && state <= 3 ? kNames[state] : "?",
            state == kUiLoading ? " (no name swaps: a match may be loading)" : "");
    return g_OrigChangeUiState(self, state);
}

static void ResolveUiState(const Section& text, const Section& rdata) {
    uint8_t* fn = FunctionStart(FindRipRef(text, FindString(rdata, "ChangeGameUIState: %s -> %s\n")));
    // push rbp; push rsi; push r14; sub rsp,40h; movsxd r14,dword [rcx+130h] (16 bytes, no RIP-relative)
    static const uint8_t kProlog[] = {0x40, 0x55, 0x56, 0x41, 0x56, 0x48, 0x83, 0xEC, 0x40,
                                      0x4C, 0x63, 0xB1, 0x30, 0x01, 0x00, 0x00};
    bool hooked = fn && memcmp(fn, kProlog, sizeof(kProlog)) == 0 &&
                  InstallHook(fn, sizeof(kProlog), (void*)&HookedChangeUiState, (void**)&g_OrigChangeUiState);
    Log("[%c] Interface state: ChangeGameUIState %p %s", hooked ? '+' : '-', (void*)fn,
        hooked ? "hooked" : fn ? "(another layout: not hooked)" : "not found");
}

// --- Terrain ---
// A terrain is another map for the world: C_DOTA_MapOverride (the engine's
// IWorldVPKOverrideManager) tells the client which map to load instead of the server's,
// from the local player's equipped terrain (its map_override: maps/dota.vmap ->
// maps/dota_desert.vmap; the game itself leaves the hero demo out). Its decision
// (self, map, out name, out item) -> bool is hooked: the preset's terrain answers for its
// map, at every map load, so a new terrain needs no restart.
using MapOverrideFn = bool (*)(void* self, const char* map, void* outName, void** outItem);
static MapOverrideFn g_OrigMapOverride = nullptr;
using BufSetLengthFn = char* (*)(void* s, int len, bool ignoreAlign, int* newCapacity);
using BufInsertFn = const char* (*)(void* s, int at, const char* str, int len, bool ignoreAlign);
static BufSetLengthFn g_BufSetLength = nullptr;
static BufInsertFn g_BufInsert = nullptr;

// "maps/dota.vmap", "maps/dota.vpk", "dota" -> "dota"
static std::string MapBase(const char* path) {
    std::string p = Normalize(path ? path : "");
    if (p.compare(0, 5, "maps/") == 0) p.erase(0, 5);
    size_t dot = p.rfind('.');
    if (dot != std::string::npos && p.find('/', dot) == std::string::npos) p.resize(dot);
    return p;
}

static bool HookedMapOverride(void* self, const char* map, void* outName, void** outItem) {
    std::string from, to;
    AcquireSRWLockShared(&g_glowsLock);
    from = g_terrainFrom;
    to = g_terrainTo;
    ReleaseSRWLockShared(&g_glowsLock);
    if (!to.empty() && map && outName && outItem && g_BufSetLength && g_BufInsert && MapBase(map) == from) {
        *outItem = nullptr;  // no item of the player's: only the map
        g_BufSetLength(outName, 0, false, nullptr);
        g_BufInsert(outName, 0, to.c_str(), -1, false);
        Log("[*] Terrain: the map %s loads as %s", from.c_str(), to.c_str());
        return true;
    }
    return g_OrigMapOverride(self, map, outName, outItem);
}

// --- HUD skin ---
// The HUD's SetHudSkin(hud, skin, ...) (what the test_hud_skin command calls) loads the
// skin's images (resource/flash3/images/hud_skins/<skin>/...) and keeps the skin's name.
// The game calls it with the equipped skin when the HUD comes up: hooked, it gets the
// preset's skin instead. A skin changed in a match is put on at once (HudSkinTick).
using SetHudSkinFn = void (*)(void* hud, const char* skin, bool a, int b, bool c);
static SetHudSkinFn g_OrigSetHudSkin = nullptr;
static void** g_hudPtr = nullptr;     // the HUD object
static int g_hudSkinField = -1;       // its current skin's name (a CUtlString)
static void* g_hudShown = nullptr;    // main thread: the HUD the preset's skin went on,
static std::string g_hudShownSkin;    // that skin,
static std::string g_hudAfter;        // and the HUD's own name for it right after

static std::string HudCurrentSkin(void* hud) {
    if (!hud || g_hudSkinField < 0) return "?";
    const char* s = *(const char* const*)((const uint8_t*)hud + g_hudSkinField);
    return s ? std::string(s) : std::string();
}

static void HookedSetHudSkin(void* hud, const char* skin, bool a, int b, bool c) {
    std::string mine;
    AcquireSRWLockShared(&g_glowsLock);
    mine = g_hudSkin;
    ReleaseSRWLockShared(&g_glowsLock);
    if (mine.empty()) {
        g_OrigSetHudSkin(hud, skin, a, b, c);
        return;
    }
    if (!skin || _stricmp(skin, mine.c_str()) != 0) Log("[*] HUD skin: %s instead of %s", mine.c_str(), skin && skin[0] ? skin : "the default");
    g_OrigSetHudSkin(hud, mine.c_str(), a, b, c);
    g_hudShown = hud;
    g_hudShownSkin = mine;
    g_hudAfter = HudCurrentSkin(hud);
}

// Main thread, in a match: the preset's skin on the HUD, again when the HUD is another one
// or something else changed its skin (a skin the game forces stays: it is not fought over)
static void HudSkinTick() {
    if (!g_OrigSetHudSkin || !g_hudPtr || g_inMenu) return;
    void* hud = *g_hudPtr;
    if (!hud) return;
    std::string mine;
    AcquireSRWLockShared(&g_glowsLock);
    mine = g_hudSkin;
    ReleaseSRWLockShared(&g_glowsLock);
    std::string cur = HudCurrentSkin(hud);
    if (hud == g_hudShown && mine == g_hudShownSkin && cur == g_hudAfter) return;
    if (mine.empty() && g_hudShownSkin.empty()) {  // nothing of ours: the game's own skin stays
        g_hudShown = hud;
        g_hudAfter = cur;
        return;
    }
    Log("[*] HUD skin: %s", mine.empty() ? "the default again" : mine.c_str());
    g_OrigSetHudSkin(hud, mine.c_str(), false, 0, false);  // "" has no images: the default skin
    g_hudShown = hud;
    g_hudShownSkin = mine;
    g_hudAfter = HudCurrentSkin(hud);
}

static void ResolveTerrainAndHud(const Section& text) {
    uint8_t* end = text.start + text.size;
    // the terrain decision: the one function asking the loadout for slot 93 (terrains) with
    // this frame: xor r9d,r9d; mov edx,1000; mov r8d,93
    static const uint8_t kTerrainSlot[] = {0x45, 0x33, 0xC9, 0xBA, 0xE8, 0x03, 0x00, 0x00, 0x41, 0xB8, 0x5D, 0x00, 0x00, 0x00};
    // mov [rsp+20h],r9; push rbx; push rdi; push r14; push r15; sub rsp,48h (15 bytes, hooked);
    // mov r15,rcx; xor ebx,ebx; mov [r9],rbx (the out item cleared)
    static const uint8_t kMapProlog[] = {0x4C, 0x89, 0x4C, 0x24, 0x20, 0x53, 0x57, 0x41, 0x56, 0x41, 0x57, 0x48,
                                         0x83, 0xEC, 0x48, 0x4C, 0x8B, 0xF9, 0x33, 0xDB, 0x49, 0x89, 0x19};
    uint8_t* mapFn = nullptr;
    for (uint8_t* p = text.start; p + sizeof(kTerrainSlot) <= end && !mapFn; p++) {
        if (memcmp(p, kTerrainSlot, sizeof(kTerrainSlot)) != 0) continue;
        uint8_t* fn = FunctionStart(p);
        if (fn && memcmp(fn, kMapProlog, sizeof(kMapProlog)) == 0) mapFn = fn;
    }
    if (HMODULE t0 = GetModuleHandleA("tier0.dll")) {
        g_BufSetLength = (BufSetLengthFn)GetProcAddress(t0, "?SetLength@CBufferString@@QEAAPEADH_NPEAH@Z");
        g_BufInsert = (BufInsertFn)GetProcAddress(t0, "?Insert@CBufferString@@QEAAPEBDHPEBDH_N@Z");
    }
    bool terrain = mapFn && g_BufSetLength && g_BufInsert &&
                   InstallHook(mapFn, 15, (void*)&HookedMapOverride, (void**)&g_OrigMapOverride);
    Log("[%c] Terrains: map override %p %s", terrain ? '+' : '-', (void*)mapFn, terrain ? "hooked" : "NOT hooked");

    // the test_hud_skin command: mov rcx,[<hud>]; xor r9d,r9d; xor r8d,r8d; mov byte [rsp+20h],0; call <SetHudSkin>
    static const uint8_t kHudCall[] = {0x45, 0x33, 0xC9, 0x45, 0x33, 0xC0, 0xC6, 0x44, 0x24, 0x20, 0x00, 0xE8};
    uint8_t* setSkin = nullptr;
    for (uint8_t* p = text.start + 7; p + sizeof(kHudCall) + 4 <= end && !setSkin; p++) {
        if (memcmp(p, kHudCall, sizeof(kHudCall)) != 0 || p[-7] != 0x48 || p[-6] != 0x8B || p[-5] != 0x0D) continue;
        g_hudPtr = (void**)(p + *(int32_t*)(p - 4));
        setSkin = p + sizeof(kHudCall) + 4 + *(int32_t*)(p + sizeof(kHudCall));
    }
    // mov [rsp+20h],r9d; mov [rsp+10h],rdx; push rbp; push rbx; push rdi; push r12 (15 bytes)
    static const uint8_t kSkinProlog[] = {0x44, 0x89, 0x4C, 0x24, 0x20, 0x48, 0x89, 0x54, 0x24, 0x10, 0x55, 0x53, 0x57, 0x41, 0x54};
    // its name kept: lea rcx,[r13+X]; mov [rsp+40h],sil; mov rdx,r12; call [CUtlString::Set]
    static const uint8_t kSkinName[] = {0x40, 0x88, 0x74, 0x24, 0x40, 0x49, 0x8B, 0xD4, 0xFF, 0x15};
    for (uint8_t* q = setSkin; setSkin && q < setSkin + 0x400 && q + 17 <= end; q++) {
        if (q[0] == 0x49 && q[1] == 0x8D && q[2] == 0x8D && memcmp(q + 7, kSkinName, sizeof(kSkinName)) == 0) {
            g_hudSkinField = *(int32_t*)(q + 3);
            break;
        }
    }
    bool hud = setSkin && memcmp(setSkin, kSkinProlog, sizeof(kSkinProlog)) == 0 &&
               InstallHook(setSkin, sizeof(kSkinProlog), (void*)&HookedSetHudSkin, (void**)&g_OrigSetHudSkin);
    if (!hud) g_hudPtr = nullptr;
    Log("[%c] HUD skins: SetHudSkin %p %s, HUD %p, skin name +0x%X", hud ? '+' : '-', (void*)setSkin,
        hud ? "hooked" : "NOT hooked", (void*)g_hudPtr, g_hudSkinField);
}

// --- Map effects (shaders) ---
// A map effect item gives materials, not files: the custom post-process material of the
// map ("map_shader"), of the portraits ("portrait_shader") and a post effect
// ("post_effect_shader"). The game sets them by name after every map load, from the
// equipped item (an empty name: none): SetMapShader(name) and its two twins load the
// material and keep it in a global the renderer reads every frame. Hooked, they get the
// preset's material; a change in the running game is set at once (ShaderTick).
using SetShaderFn = void (*)(const char* name);
static SetShaderFn g_OrigSetShader[kShaderKinds] = {};
static std::string g_shaderShown[kShaderKinds];  // main thread: what we set last
static bool g_shaderOurs[kShaderKinds] = {};     // a material of the preset's is on

static const char* const kShaderNames[kShaderKinds] = {"map", "portrait", "post effect"};

static void SetShaderHooked(int kind, const char* name) {
    std::string mine;
    AcquireSRWLockShared(&g_glowsLock);
    mine = g_shaders[kind];
    ReleaseSRWLockShared(&g_glowsLock);
    if (mine.empty()) {
        g_OrigSetShader[kind](name);
        g_shaderOurs[kind] = false;
        g_shaderShown[kind].clear();
        return;
    }
    if (!name || _stricmp(name, mine.c_str()) != 0)
        Log("[*] Map effect: %s shader %s", kShaderNames[kind], mine.c_str());
    g_OrigSetShader[kind](mine.c_str());
    g_shaderOurs[kind] = true;
    g_shaderShown[kind] = mine;
}
static void HookedSetShaderMap(const char* name) { SetShaderHooked(kShaderMap, name); }
static void HookedSetShaderPortrait(const char* name) { SetShaderHooked(kShaderPortrait, name); }
static void HookedSetShaderPost(const char* name) { SetShaderHooked(kShaderPost, name); }

// Main thread, every script tick in a match: a material changed in the preset is set at
// once; one taken out of the preset leaves no custom material (the game's next map load
// sets the player's own again). The menu is left to the game: it sets them on map loads.
static void ShaderTick() {
    if (g_inMenu) return;
    for (int k = 0; k < kShaderKinds; k++) {
        if (!g_OrigSetShader[k]) continue;
        std::string mine;
        AcquireSRWLockShared(&g_glowsLock);
        mine = g_shaders[k];
        ReleaseSRWLockShared(&g_glowsLock);
        if (mine == g_shaderShown[k] && (!mine.empty() || !g_shaderOurs[k])) continue;
        if (mine.empty() && !g_shaderOurs[k]) continue;
        Log("[*] Map effect: %s shader %s", kShaderNames[k], mine.empty() ? "back to none" : mine.c_str());
        g_OrigSetShader[k](mine.c_str());
        g_shaderOurs[k] = !mine.empty();
        g_shaderShown[k] = mine;
    }
}

static void ResolveShaders(const Section& text) {
    // the map effects' setup ends with the three: mov rcx,rdi; call <portrait>;
    // mov rcx,rbp; call <map>; mov rcx,r13; call <post effect>
    uint8_t* end = text.start + text.size;
    uint8_t* site = nullptr;
    for (uint8_t* p = text.start; p + 24 <= end && !site; p++) {
        if (p[0] == 0x48 && p[1] == 0x8B && p[2] == 0xCF && p[3] == 0xE8 && p[8] == 0x48 && p[9] == 0x8B && p[10] == 0xCD &&
            p[11] == 0xE8 && p[16] == 0x49 && p[17] == 0x8B && p[18] == 0xCD && p[19] == 0xE8)
            site = p;
    }
    // mov rax,rsp; push rdi; sub rsp,100h; test rcx,rcx (14 bytes, no RIP-relative)
    static const uint8_t kProlog[] = {0x48, 0x8B, 0xC4, 0x57, 0x48, 0x81, 0xEC, 0x00, 0x01, 0x00, 0x00, 0x48, 0x85, 0xC9};
    static void* const kHooks[kShaderKinds] = {(void*)&HookedSetShaderMap, (void*)&HookedSetShaderPortrait, (void*)&HookedSetShaderPost};
    // call offsets in the site: portrait at +3, map at +11, post effect at +19
    static const int kCall[kShaderKinds] = {11, 3, 19};
    uint8_t* fn[kShaderKinds] = {};
    int hooked = 0;
    for (int k = 0; site && k < kShaderKinds; k++) {
        uint8_t* call = site + kCall[k];
        fn[k] = call + 5 + *(int32_t*)(call + 1);
        if (memcmp(fn[k], kProlog, sizeof(kProlog)) == 0 &&
            InstallHook(fn[k], sizeof(kProlog), kHooks[k], (void**)&g_OrigSetShader[k]))
            hooked++;
        else
            g_OrigSetShader[k] = nullptr;
    }
    Log("[%c] Map effects: map %p, portrait %p, post effect %p (%d of 3 hooked)", hooked == kShaderKinds ? '+' : '-',
        (void*)fn[kShaderMap], (void*)fn[kShaderPortrait], (void*)fn[kShaderPost], hooked);
}

// --- Styles: material groups ---
// An item's style is often a material group of its model ("skin" 1: the model's group "1",
// the Windranger arcana's autumn look, of her hair too), and a style picked while the
// model was loaded did not show: a model takes its materials when it loads. The game
// switches a group on the model itself: CSkeletonInstance's m_materialGroup (a string
// token) through its change handler, which tells the scene object. The config's
// matgroup|model|group lines are the preset's groups; the models in the world get theirs
// (in a match your own hero's only, and the strategy screen's previews).
using MakeTokenFn = uint32_t* (*)(uint32_t* out, const char* s);
using GroupChangedFn = void (*)(void* skeleton, void* unused, const uint32_t* token);
static MakeTokenFn g_MakeToken = nullptr;
static GroupChangedFn g_GroupChanged = nullptr;
static int g_groupOffset = -1;
// CModelState's m_MeshGroupMask (a uint64: bit N, the model's mesh group N) through its
// change handler ("skeletonMeshGroupMaskChanged"), which tells the scene object: (model
// state, entity, &mask)
using MaskChangedFn = void (*)(void* modelState, void* entity, const uint64_t* mask);
static MaskChangedFn g_MaskChanged = nullptr;
static int g_maskOffset = -1;

static void ResolveMatGroups(const Section& text, const Section& rdata) {
    // the handler registered as "skeletonMaterialGroupChanged": lea rax,<handler> before it;
    // the handler: mov r8d,[r8]; cmp r8d,[rcx+<m_materialGroup>]
    uint8_t* reg = FindRipRef(text, FindString(rdata, "skeletonMaterialGroupChanged"));
    for (uint8_t* q = reg; reg && q > reg - 0x60 && !g_GroupChanged; q--) {
        if (q[0] != 0x48 || q[1] != 0x8D || q[2] != 0x05) continue;
        uint8_t* fn = q + 7 + *(int32_t*)(q + 3);
        if (fn < text.start || fn + 10 > text.start + text.size) continue;
        if (fn[0] == 0x45 && fn[1] == 0x8B && fn[2] == 0x00 && fn[3] == 0x44 && fn[4] == 0x3B && fn[5] == 0x81) {
            g_GroupChanged = (GroupChangedFn)fn;
            g_groupOffset = *(int32_t*)(fn + 6);
        }
    }
    if (HMODULE t0 = GetModuleHandleA("tier0.dll"))
        g_MakeToken = (MakeTokenFn)GetProcAddress(t0, "?Make@CUtlStringToken@@SA?AV1@PEBD@Z");
    bool ok = g_GroupChanged && g_MakeToken && g_groupOffset > 0 && g_groupOffset < 0x2000;
    if (!ok) g_GroupChanged = nullptr;
    Log("[%c] Style groups: handler %p, m_materialGroup +0x%X, token %p", ok ? '+' : '-', (void*)g_GroupChanged,
        g_groupOffset, (void*)g_MakeToken);
    // the mask's handler: lea rax,<handler> before its name; it begins mov [rsp+8],rbx;
    // mov [rsp+10h],rsi; push rdi; sub rsp,20h; mov rax,[r8]; mov rdi,r8;
    // mov [rcx+<raw>],rax; mov rsi,rdx; mov rax,[r8]; mov rbx,rcx; cmp [rcx+<mask>],rax
    static const uint8_t kMask[] = {0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x74, 0x24, 0x10, 0x57, 0x48,
                                    0x83, 0xEC, 0x20, 0x49, 0x8B, 0x00, 0x49, 0x8B, 0xF8, 0x48, 0x89, 0x81};
    static const uint8_t kMask2[] = {0x48, 0x8B, 0xF2, 0x49, 0x8B, 0x00, 0x48, 0x8B, 0xD9, 0x48, 0x39, 0x81};
    uint8_t* maskReg = FindRipRef(text, FindString(rdata, "skeletonMeshGroupMaskChanged"));
    for (uint8_t* q = maskReg; maskReg && q > maskReg - 0x60 && !g_MaskChanged; q--) {
        if (q[0] != 0x48 || q[1] != 0x8D || q[2] != 0x05) continue;
        uint8_t* fn = q + 7 + *(int32_t*)(q + 3);
        if (fn < text.start || fn + 0x40 > text.start + text.size) continue;
        if (memcmp(fn, kMask, sizeof kMask) == 0 && memcmp(fn + 28, kMask2, sizeof kMask2) == 0) {
            g_MaskChanged = (MaskChangedFn)fn;
            g_maskOffset = *(int32_t*)(fn + 40);
        }
    }
    if (g_maskOffset <= 0 || g_maskOffset >= 0x1000) g_MaskChanged = nullptr;
    Log("[%c] Style meshes: handler %p, m_MeshGroupMask +0x%X (model state +0x%X)", g_MaskChanged ? '+' : '-',
        (void*)g_MaskChanged, g_maskOffset, g_ih.modelState);
}

// Main thread, every glow tick (a few times a second at most)
// Whether a model (a hero's or one of its pieces) is of a hero the game dresses itself in
// the menu (its arcana's skin, level and effects are the game's then)
static bool DressedEntity(const uint8_t* ent, const uint8_t* node) {
    static std::unordered_map<const void*, bool> npcs;  // (the interface thread's only)
    if (g_unitNameOffset <= 0 || !ent || !node) return false;
    const uint8_t* hero = ent;
    if (!IsA(ent, ".?AVC_DOTA_BaseNPC@@", npcs)) {
        const uint8_t* parentNode = *(const uint8_t* const*)(node + g_ih.parent);
        hero = parentNode ? *(const uint8_t* const*)(parentNode + g_ih.owner) : nullptr;
        if (!hero || !IsA(hero, ".?AVC_DOTA_BaseNPC@@", npcs)) return false;
    }
    const char* unit = UnitNameOf(hero);
    if (!unit || !*unit) return false;
    AcquireSRWLockShared(&g_personaLock);
    bool dressed = g_dressedNpcs.count(unit) > 0;
    ReleaseSRWLockShared(&g_personaLock);
    return dressed;
}

static void MatGroupTick() {
    if ((!g_GroupChanged && !g_MaskChanged) || !g_itemHostsReady || !g_entityList) return;
    static ULONGLONG last = 0;
    ULONGLONG now = GetTickCount64();
    if (now - last < 200) return;
    last = now;
    std::unordered_map<std::string, std::string> groups;
    std::unordered_map<std::string, MeshMask> masks;
    AcquireSRWLockShared(&g_glowsLock);
    if (g_GroupChanged) groups = g_matGroups;
    if (g_MaskChanged) masks = g_meshMasks;
    ReleaseSRWLockShared(&g_glowsLock);
    static std::set<std::string> applied;  // models given a group: put back when it leaves
    // models given a mask: choice 0 again when they leave
    static std::map<std::string, MeshMask> appliedMasks;
    static std::set<std::string> logged;
    if (groups.empty() && applied.empty() && masks.empty() && appliedMasks.empty()) return;
    static std::unordered_map<std::string, uint32_t> tokens;
    // the entities given a material group: entity handle -> the model it was given for. One
    // that wears another model since keeps it otherwise: the hero page keeps its hero when an
    // item changes, and Razor's body made his own again with the arcana's group was not drawn
    static std::unordered_map<uint32_t, std::string> grouped;
    std::set<uint32_t> seen;
    uint8_t** chunks = (uint8_t**)*g_entityList;
    for (int c = 0; chunks && c < 64; c++) {
        uint8_t* chunk = chunks[c];
        if (!chunk) continue;
        for (int i = 0; i < 512; i++) {
            uint8_t* e = chunk + i * 0x70;
            const uint8_t* ent = *(const uint8_t* const*)e;
            int index = c * 512 + i;
            if (!ent || (int)(*(const uint32_t*)(e + 0x10) & 0x7fff) != index) continue;
            const uint8_t* node = nullptr;
            std::string model = ModelOf(ent, &node);
            if (model.empty() || !node) continue;
            uint32_t handle = *(const uint32_t*)(e + 0x10);
            seen.insert(handle);
            // the model the file holds: the swap table's replacement of it
            std::string key = model;
            AcquireSRWLockShared(&g_rulesLock);
            if (const std::string* to = RuleFor(*g_rules, model)) key = *to;
            ReleaseSRWLockShared(&g_rulesLock);
            auto g = groups.find(key);
            auto gr = grouped.find(handle);
            if (gr != grouped.end() && gr->second != key) {
                if (g == groups.end() && g_GroupChanged) {
                    uint32_t cur = *(const uint32_t*)(node + g_groupOffset), zero = 0;
                    if (cur != 0) {
                        g_GroupChanged((void*)node, nullptr, &zero);
                        Log("[*] Style: entity %d now %s (was %s): group %08x -> its own", index, key.c_str(), gr->second.c_str(), cur);
                    }
                }
                grouped.erase(gr);
            }
            auto mk = masks.find(key);
            auto am = appliedMasks.find(key);
            bool matWork = g != groups.end() || applied.count(key);
            bool maskWork = mk != masks.end() || am != appliedMasks.end();
            if (!matWork && !maskWork) continue;
            // a hero the game dresses itself in the menu: its skins and arcana level are the game's
            if (g_inMenu && DressedEntity(ent, node)) continue;
            if (!g_inMenu && index < 0x4000) {
                // a match: your hero and what it wears
                const uint8_t* parentNode = *(const uint8_t* const*)(node + g_ih.parent);
                int holder = parentNode ? EntityIndexOf(*(const uint8_t* const*)(parentNode + g_ih.owner)) : index;
                if (!(g_heroIndex > 0 && holder == g_heroIndex)) continue;
            }
            if (maskWork) {
                uint8_t* state = (uint8_t*)node + g_ih.modelState;
                uint64_t cur = *(const uint64_t*)(state + g_maskOffset);
                uint64_t want = mk != masks.end() ? (cur & ~mk->second.clear) | mk->second.set
                                                  : (cur & ~am->second.clear) | am->second.reset;
                if (cur != want) {
                    g_MaskChanged(state, (void*)ent, &want);
                    uint64_t after = *(const uint64_t*)(state + g_maskOffset);
                    char tag[32];
                    snprintf(tag, sizeof tag, "|%llx", (unsigned long long)want);
                    if (logged.insert(key + tag).second)
                        Log("[*] Style: %s mesh mask %llx -> %llx (now %llx)", key.c_str(), (unsigned long long)cur,
                            (unsigned long long)want, (unsigned long long)after);
                }
                if (mk != masks.end()) appliedMasks[key] = mk->second;
                else appliedMasks.erase(key);
            }
            if (!matWork || !g_GroupChanged) continue;
            uint32_t want = 0;  // 0: the model's own group again
            if (g != groups.end()) {
                auto tk = tokens.find(g->second);
                if (tk == tokens.end()) {
                    uint32_t tok = 0;
                    g_MakeToken(&tok, g->second.c_str());
                    tk = tokens.emplace(g->second, tok).first;
                }
                want = tk->second;
            }
            uint32_t cur = *(const uint32_t*)(node + g_groupOffset);
            if (cur == want) continue;
            g_GroupChanged((void*)node, nullptr, &want);
            {
                static std::set<std::string> told;
                uint32_t after = *(const uint32_t*)(node + g_groupOffset);
                char tag[64];
                snprintf(tag, sizeof tag, "|%08x|%08x", cur, want);
                if (told.insert(key + tag).second)
                    Log("[*] Style: %s group %08x -> %08x (%s), now %08x", key.c_str(), cur, want,
                        g != groups.end() ? g->second.c_str() : "own", after);
            }
            if (want) grouped[handle] = key;
            if (want && applied.insert(key).second)
                Log("[*] Style: %s gets material group %s", key.c_str(), g->second.c_str());
        }
    }
    for (auto it = grouped.begin(); it != grouped.end();) it = seen.count(it->first) ? std::next(it) : grouped.erase(it);
}

// --- Prismatic gems ---
// A hero's gem colour is C_DOTA_BaseNPC's m_colorGemColor (with m_bHasColorGem), worked out
// from its items' prismatic gems when they change (the Fractal Horns' own red without one).
// Its effects take it when they are made: the hero's virtual "colour this effect" sets
// control point 15 to the colour and 16 to x=1 (0: the effect's own colours). A colour
// picked on the item page (the config's gem|model|colour) is the hero's: the hook gives it
// to the effects made from now on, GemTick to the hero and to the effects already made
// (their indices kept by the hook: a gone effect's index is simply not found).
using ApplyGemFn = void (*)(void* npc, int fx, const uint32_t* skip);
static ApplyGemFn g_OrigApplyGem = nullptr;
static int g_gemOffset = -1, g_hasGemOffset = -1;
struct GemFx { const void* npc; int fx; bool hasSkip; uint32_t skip; };
static std::vector<GemFx> g_gemFx;
static std::unordered_map<const void*, uint32_t> g_gemWant;  // hero -> its colour, from GemTick
static SRWLOCK g_gemLock = SRWLOCK_INIT;

// The colour picked for a hero entity: its model's, or that of the model the swap table
// turns it into (in a match your own hero's and its illusions' only)
static bool GemFor(const uint8_t* ent, uint32_t& color, std::string* keyOut = nullptr) {
    const uint8_t* node = nullptr;
    std::string model = ModelOf(ent, &node);
    if (model.empty()) return false;
    std::string swapped;
    AcquireSRWLockShared(&g_rulesLock);
    if (const std::string* to = RuleFor(*g_rules, model)) swapped = *to;
    ReleaseSRWLockShared(&g_rulesLock);
    bool found = false;
    std::string key;
    AcquireSRWLockShared(&g_glowsLock);
    auto g = g_gems.find(model);
    if (g == g_gems.end() && !swapped.empty()) g = g_gems.find(swapped);
    if (g != g_gems.end()) {
        color = g->second;
        key = g->first;
        found = true;
    }
    ReleaseSRWLockShared(&g_glowsLock);
    if (!found) return false;
    if (!g_inMenu) {
        int index = EntityIndexOf(ent);
        if (index >= 0 && index < 0x4000) {
            int owner = (int)(*(const uint32_t*)(ent + g_ih.ownerEntity) & 0x7fff);
            if (!(g_heroIndex > 0 && (index == g_heroIndex || owner == g_heroIndex))) return false;
        }
    }
    if (keyOut) *keyOut = key;
    return true;
}

// The game works the colour out from the hero's items (its prismatic gem) when they change,
// and its items' effects made then read it at once (the weapon's: a single particle coloured
// when it is born): the picked colour is the answer from the start
using ComputeGemFn = bool (*)(void* npc, uint32_t* out);
static ComputeGemFn g_OrigComputeGem = nullptr;

static bool HookedComputeGem(void* npc, uint32_t* out) {
    bool has = g_OrigComputeGem(npc, out);
    uint32_t c = 0;
    if (g_itemHostsReady && out && GemFor((const uint8_t*)npc, c)) {
        *out = c;
        return true;
    }
    return has;
}

static void HookedApplyGem(void* npc, int fx, const uint32_t* skip) {
    AcquireSRWLockExclusive(&g_gemLock);
    if (g_gemFx.size() >= 4096) g_gemFx.erase(g_gemFx.begin(), g_gemFx.begin() + 1024);
    g_gemFx.push_back({npc, fx, skip != nullptr, skip ? *skip : 0u});
    auto w = g_gemWant.find(npc);
    if (w != g_gemWant.end()) {
        *(uint32_t*)((uint8_t*)npc + g_gemOffset) = w->second;
        *((uint8_t*)npc + g_hasGemOffset) = 1;
    }
    ReleaseSRWLockExclusive(&g_gemLock);
    g_OrigApplyGem(npc, fx, skip);
}

static void ResolveGems(const Section& text, const Section& rdata, const Section& data) {
    SchemaField want[] = {
        {"C_DOTA_BaseNPC", "m_colorGemColor", &g_gemOffset},
        {"C_DOTA_BaseNPC", "m_bHasColorGem", &g_hasGemOffset},
    };
    ReadSchemaOffsets(rdata, data, want, sizeof(want) / sizeof(want[0]));
    uint8_t* fn = nullptr;
    if (g_gemOffset > 0 && g_hasGemOffset > 0) {
        // mov [rsp+10h],rbx; mov [rsp+18h],rsi; push rdi; sub rsp,60h; mov ebx,[rcx+<colour>];
        // mov edi,edx; ...; cmp byte [rcx+<has gem>],0
        uint8_t sig[23] = {0x48, 0x89, 0x5C, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18, 0x57, 0x48,
                           0x83, 0xEC, 0x60, 0x8B, 0x99, 0, 0, 0, 0, 0x8B, 0xFA};
        *(int32_t*)(sig + 17) = g_gemOffset;
        for (size_t i = 0; i + sizeof sig + 0x40 <= text.size && !fn; i++) {
            uint8_t* p = text.start + i;
            if (memcmp(p, sig, sizeof sig) != 0) continue;
            for (uint8_t* q = p + sizeof sig; q < p + sizeof sig + 0x30; q++)
                if (q[0] == 0x80 && q[1] == 0xB9 && *(int32_t*)(q + 2) == g_hasGemOffset && q[6] == 0) { fn = p; break; }
        }
    }
    // the first 15 bytes (to sub rsp) have no RIP-relative instructions
    bool ok = fn && InstallHook(fn, 15, (void*)HookedApplyGem, (void**)&g_OrigApplyGem);
    if (!ok) g_OrigApplyGem = nullptr;
    Log("[%c] Gems: effect colouring %p, m_colorGemColor +0x%X, m_bHasColorGem +0x%X", ok ? '+' : '-', (void*)fn,
        g_gemOffset, g_hasGemOffset);
    // the colour worked out from the items: lea rdx,[reg+<colour>]; mov rcx,reg; call <it>;
    // mov [reg+<has gem>],al. It begins mov [rsp+10h],rbx; mov [rsp+18h],rbp; mov [rsp+20h],rsi
    static const uint8_t kCompute[] = {0x48, 0x89, 0x5C, 0x24, 0x10, 0x48, 0x89, 0x6C, 0x24, 0x18, 0x48, 0x89, 0x74, 0x24, 0x20};
    uint8_t* compute = nullptr;
    for (size_t i = 0; ok && i + 0x20 <= text.size && !compute; i++) {
        uint8_t* q = text.start + i;
        if ((q[0] & 0xFB) != 0x48 || q[1] != 0x8D || (q[2] & 0xF8) != 0x90 || *(int32_t*)(q + 3) != g_gemOffset) continue;
        for (uint8_t* c = q + 7; c < q + 7 + 6; c++) {
            if (*c != 0xE8 || c[5] != 0x88 || (c[6] & 0xF8) != 0x80 || *(int32_t*)(c + 7) != g_hasGemOffset) continue;
            uint8_t* fn2 = c + 5 + *(int32_t*)(c + 1);
            if (fn2 >= text.start && fn2 + 0x40 <= text.start + text.size && memcmp(fn2, kCompute, sizeof kCompute) == 0) compute = fn2;
            break;
        }
    }
    bool ok2 = compute && InstallHook(compute, 15, (void*)HookedComputeGem, (void**)&g_OrigComputeGem);
    if (!ok2) g_OrigComputeGem = nullptr;
    Log("[%c] Gems: hero colour from its items %p", ok2 ? '+' : '-', (void*)compute);
}

// Main thread, every glow tick
static void GemTick() {
    if (!g_OrigApplyGem || !g_itemHostsReady || !g_entityList) return;
    static ULONGLONG last = 0;
    ULONGLONG now = GetTickCount64();
    if (now - last < 200) return;
    last = now;
    std::unordered_map<std::string, uint32_t> gems;
    AcquireSRWLockShared(&g_glowsLock);
    gems = g_gems;
    ReleaseSRWLockShared(&g_glowsLock);
    // heroes given a colour: their own (put back when it leaves), the one their effects show
    struct Given { uint32_t color; uint8_t has; uint32_t shown; };
    static std::unordered_map<const void*, Given> given;
    static std::unordered_map<const void*, bool> npcs;
    static std::set<std::string> logged;
    if (gems.empty() && given.empty()) return;
    std::unordered_map<const void*, uint32_t> want;
    std::vector<std::pair<uint8_t*, uint32_t>> recolor;  // hero, the colour for its effects
    std::set<const void*> alive;  // every entity in the list (a hero's model may be loading)
    uint8_t** chunks = (uint8_t**)*g_entityList;
    for (int c = 0; chunks && c < 64; c++) {
        uint8_t* chunk = chunks[c];
        if (!chunk) continue;
        for (int i = 0; i < 512; i++) {
            uint8_t* e = chunk + i * 0x70;
            const uint8_t* ent = *(const uint8_t* const*)e;
            int index = c * 512 + i;
            if (!ent || (int)(*(const uint32_t*)(e + 0x10) & 0x7fff) != index) continue;
            alive.insert(ent);
            if (!IsA(ent, ".?AVC_DOTA_BaseNPC@@", npcs)) continue;
            uint32_t pick = 0;
            std::string key;
            bool ours = GemFor(ent, pick, &key);
            auto g = ours ? gems.find(key) : gems.end();
            uint8_t* npc = (uint8_t*)ent;
            uint32_t* color = (uint32_t*)(npc + g_gemOffset);
            uint8_t* has = npc + g_hasGemOffset;
            auto gv = given.find(ent);
            if (g != gems.end() && ours) {
                want[ent] = g->second;
                if (gv == given.end()) gv = given.emplace(ent, Given{*color, *has, *has ? *color : 0u}).first;
                // the game works it out again when the hero's items change: ours again
                if (*color != g->second || !*has) {
                    *color = g->second;
                    *has = 1;
                }
                if (gv->second.shown != g->second) {
                    gv->second.shown = g->second;
                    recolor.push_back({npc, g->second});
                    char tag[96];
                    snprintf(tag, sizeof tag, "|%08x", g->second);
                    if (logged.insert(g->first + tag).second)
                        Log("[*] Gem: %s gets colour %02X%02X%02X", g->first.c_str(), g->second & 0xFF,
                            (g->second >> 8) & 0xFF, (g->second >> 16) & 0xFF);
                }
            } else if (gv != given.end()) {
                // the colour left: the hero's own again, on its effects too
                *color = gv->second.color;
                *has = gv->second.has;
                recolor.push_back({npc, gv->second.color});
                given.erase(gv);
            }
        }
    }
    for (auto it = given.begin(); it != given.end();) it = alive.count(it->first) ? std::next(it) : given.erase(it);
    std::vector<GemFx> fx;
    AcquireSRWLockExclusive(&g_gemLock);
    g_gemWant.swap(want);
    g_gemFx.erase(std::remove_if(g_gemFx.begin(), g_gemFx.end(), [&](const GemFx& f) { return !alive.count(f.npc); }),
                  g_gemFx.end());
    if (!recolor.empty()) fx = g_gemFx;
    ReleaseSRWLockExclusive(&g_gemLock);
    for (auto& [npc, c] : recolor) {
        int n = 0;
        for (auto& f : fx)
            if (f.npc == npc) {
                g_OrigApplyGem(npc, f.fx, f.hasSkip ? &f.skip : nullptr);
                n++;
            }
        if (n) Log("[*] Gem: %d effects of %p coloured %02X%02X%02X", n, (void*)npc, c & 0xFF, (c >> 8) & 0xFF, (c >> 16) & 0xFF);
    }
}

// --- Weather ---
// A weather item has no file to swap: the client turns its weather on by type, from
// cl_weather when it is set, else from the loadout. Its environment update reads the
// convar every time, takes away the effect of another weather and creates the new one as
// soon as that is loaded. The launcher's autoexec.cfg line sets it when Dota starts; the
// config's weather|N sets it in the running game, so a new weather needs no restart. The
// value is written in place: cl_weather is a cheat convar, refused by the console in a match.
static uint8_t* g_weatherConVar = nullptr;       // the ConVar object: +8 its data
static int g_weatherValueOffset = -1;            // the value in the data, as the update reads it
static const char* const* g_weatherFx = nullptr;  // the effect of each weather type (0..9)
static bool g_weatherOurs = false;               // main thread: a weather of ours is on

static void ResolveWeather(HMODULE cl, const Section& text, const Section& rdata) {
    // lea rdx,"cl_weather"; lea rcx,<convar>; call <register>
    uint8_t* reg = FindRipRef(text, FindString(rdata, "cl_weather"));
    if (reg && reg[0] == 0x48 && reg[1] == 0x8D && reg[2] == 0x15 && reg[7] == 0x48 && reg[8] == 0x8D && reg[9] == 0x0D)
        g_weatherConVar = reg + 14 + *(int32_t*)(reg + 10);
    // its readers: mov rax,[convar+8]; mov r32,[rax+disp8]. After one, the effect of the type:
    // lea rcx,<client.dll>; mov r14,[rcx+rax*8+table]
    for (size_t i = 0; g_weatherConVar && i + 10 <= text.size; i++) {
        uint8_t* p = text.start + i;
        if (p[0] != 0x48 || p[1] != 0x8B || p[2] != 0x05 || p + 7 + *(int32_t*)(p + 3) != g_weatherConVar + 8 ||
            p[7] != 0x8B || (p[8] & 0xC7) != 0x40)
            continue;
        if (g_weatherValueOffset < 0) g_weatherValueOffset = p[9];
        for (uint8_t* q = p + 10; !g_weatherFx && q < p + 0x60 && q + 15 <= text.start + text.size; q++) {
            if (q[0] == 0x48 && q[1] == 0x8D && q[2] == 0x0D && q + 7 + *(int32_t*)(q + 3) == (uint8_t*)cl &&
                q[7] == 0x4C && q[8] == 0x8B && q[9] == 0xB4 && q[10] == 0xC1) {
                auto table = (const char* const*)((uint8_t*)cl + *(int32_t*)(q + 11));
                // weather 0 has none, the others are effects
                if (!table[0] && table[1] && strncmp(table[1], "particles/", 10) == 0) g_weatherFx = table;
            }
        }
    }
    Log("[%c] Weather: cl_weather %p, value +0x%X, effects %p", g_weatherConVar && g_weatherValueOffset >= 0 ? '+' : '-',
        (void*)g_weatherConVar, g_weatherValueOffset, (void*)g_weatherFx);
}

// Main thread, every script tick: the preset's weather in cl_weather. No weather in the
// preset leaves the player's own setting, unless a weather of ours is to be taken away.
static void WeatherTick() {
    LONG want = g_weatherWanted;
    if (want < 0 || !g_weatherConVar || g_weatherValueOffset < 0) return;
    uint8_t* data = *(uint8_t**)(g_weatherConVar + 8);
    if (!data) return;
    int* value = (int*)(data + g_weatherValueOffset);
    if (*value == want) {
        if (want) g_weatherOurs = true;
        return;
    }
    if (want == 0 && !g_weatherOurs) return;
    // the new weather's effect loaded first: the game creates it only once it is loaded
    // (a match loads the weather it starts with)
    if (want > 0 && g_weatherFx && g_weatherFx[want] && g_SetResourceName && g_BlockingLoad && g_ResourceSystem) {
        ResourceName name;
        if (g_SetResourceName(&name, g_weatherFx[want])) g_BlockingLoad(g_ResourceSystem, &name, 0);
    }
    Log("[*] Weather: cl_weather %d -> %ld", *value, want);
    *value = (int)want;
    g_weatherOurs = want != 0;
}

// Main thread, every glow tick: the preset emblem's attack effect loaded for the hooked
// projectiles (into the engine's just-in-time manifest, as the stand-ins: it was loaded in
// the menu already, so this only gives its binding), looked at again every few seconds.
static void AttackFxTick() {
    if (!g_trackingHooked || g_inMenu) return;
    std::string fx;
    AcquireSRWLockShared(&g_glowsLock);
    fx = g_attackFx;
    ReleaseSRWLockShared(&g_glowsLock);
    ULONGLONG now = GetTickCount64();
    if (fx == g_attackBound && (fx.empty() || now - g_attackChecked < 3000)) return;
    bool fresh = fx != g_attackBound;
    g_attackBound = fx;
    g_attackChecked = now;
    if (fx.empty()) {
        g_attackBinding = nullptr;
        return;
    }
    if (!g_SetResourceName || !g_BlockingLoad || !g_ResourceSystem) return;
    ResourceName name;
    if (!g_SetResourceName(&name, fx.c_str())) {
        g_attackBinding = nullptr;
        return;
    }
    auto status = (GetStatusFn)(*(void***)g_ResourceSystem)[kGetStatusSlot];
    if (!fresh && g_attackBinding && status(g_ResourceSystem, &name) == kResourceResident) return;
    void* binding = g_BlockingLoad(g_ResourceSystem, &name, 0);
    if (status(g_ResourceSystem, &name) != kResourceResident) binding = nullptr;
    if (fresh || binding != g_attackBinding)
        Log("[%c] Emblem attack effect %s: %s", binding ? '+' : '-', fx.c_str(), binding ? "ready" : "not loaded");
    g_attackBinding = binding;
}

static int32_t CreateGlow(void* mgr, const std::string& fx, void* ent, const std::vector<GlowCp>& cps, void* parent = nullptr) {
    ParticleDesc d{};
    d.name = fx.c_str();
    d.attach = 2;  // PATTACH_CUSTOMORIGIN: its control points place it
    d.entity = ent;
    d.m3c = d.m40 = -1.0f;
    d.f44 = d.f48 = d.f4c = d.f64 = d.f68 = d.f6c = FLT_MAX;
    d.m60 = -1;
    int32_t out = -1;
    g_CreateParticle(mgr, &out, &d);
    if (out < 0) return -1;
    static const float zero[3] = {0, 0, 0};
    for (auto& c : cps) {
        if (c.world) g_SetControl(mgr, out, c.cp, c.vec, 0.0f);
        else g_SetControlEnt(mgr, out, c.cp, c.parent && parent ? parent : ent, c.attach, c.att.c_str(), zero, true, nullptr);
    }
    return out;
}

// key ("h<handle>|fx" on an item model, "e<index>|fx" on the hero) -> effect
static std::map<std::string, int32_t> g_matchFx;
static LONG g_matchFxVersion = -1;

// the entity a glow hangs on still exists (destroying the effect of a removed entity
// crashes the game: then it is only forgotten, the game removed it with the entity)
static bool MatchHostAlive(const std::string& key) {
    if (key.empty()) return false;
    if (key[0] == 'h') {
        uint32_t handle = (uint32_t)strtoul(key.c_str() + 1, nullptr, 10);
        for (auto& h : g_hosts) if (h.handle == handle) return true;
        return false;
    }
    return EntityByIndex(atoi(key.c_str() + 1)) != nullptr;
}

// Interface thread, after every scan in a match: your hero's glows on its item models
// (or, an item model not found, the parts the hero's own skeleton can carry)
static void MatchGlowTick() {
    if (!g_particlesReady || !g_itemHostsReady) return;
    if (g_inMenu) {
        g_matchFx.clear();  // the match's entities are gone, and their effects with them
        return;
    }
    void* mgr = g_ParticleMgr();
    if (!mgr) return;
    std::vector<GlowDef> defs;
    AcquireSRWLockShared(&g_glowsLock);
    defs = g_glowDefs;
    LONG version = g_glowsVersion;
    ReleaseSRWLockShared(&g_glowsLock);
    std::set<std::string> alive;
    void* hero = g_heroIndex > 0 ? EntityByIndex(g_heroIndex) : nullptr;
    // a new glow table: all made again
    bool fresh = version != g_matchFxVersion;
    g_matchFxVersion = version;
    if (hero && !g_heroUnit.empty() && !fresh) {
        for (auto& d : defs) {
            // "*": an effect for whichever hero you play (the emblem)
            if (d.npc != g_heroUnit && d.npc != "*") continue;
            if (d.place == 1) continue;  // the item spawns it in the menu's preview only
            const ItemHost* host = nullptr;
            for (auto& h : g_hosts)
                if (h.key == d.item && (h.parent == g_heroIndex || h.owner == g_heroIndex)) { host = &h; break; }
            std::vector<std::string> fxs;
            if (host || !d.hasParts) fxs.push_back(d.fx);
            else fxs = d.parts;
            void* on = host ? (void*)host->ent : hero;
            std::string prefix = host ? "h" + std::to_string(host->handle) : "e" + std::to_string(g_heroIndex);
            for (auto& fx : fxs) {
                std::string key = prefix + "|" + fx;
                alive.insert(key);
                if (g_matchFx.count(key)) continue;
                int32_t idx = CreateGlow(mgr, fx, on, d.cps, hero);
                g_matchFx[key] = idx;
                Log("[*] Match glow %s on %s: effect %d", fx.c_str(), host ? "the item model" : "the hero", idx);
            }
        }
    }
    // the hero's model on the strategy screen (and the pick screen): a client-side entity
    // (index from 16384, never another player's networked hero), its item models get
    // their glows as well
    if (!fresh) {
        for (auto& h : g_hosts) {
            int holder = h.parent >= 0 ? h.parent : h.owner;
            if (holder < 0x4000) continue;
            for (auto& d : defs) {
                if (d.item.empty() || d.item != h.key) continue;
                if (d.place == 2) continue;  // the item spawns it in the game only
                // an ally's model on the pick screen wears its own items
                std::string local;
                AcquireSRWLockShared(&g_rulesLock);
                local = g_localNpc;
                ReleaseSRWLockShared(&g_rulesLock);
                if (d.npc != local && d.npc != g_heroUnit) continue;
                std::string key = "h" + std::to_string(h.handle) + "|" + d.fx;
                alive.insert(key);
                if (g_matchFx.count(key)) continue;
                int32_t idx = CreateGlow(mgr, d.fx, h.ent, d.cps, EntityByIndex(holder));
                g_matchFx[key] = idx;
                Log("[*] Match glow %s on the preview's item model (entity %d): effect %d", d.fx.c_str(), h.index, idx);
            }
        }
    }
    for (auto it = g_matchFx.begin(); it != g_matchFx.end();) {
        if (alive.count(it->first)) { ++it; continue; }
        if (it->second >= 0 && MatchHostAlive(it->first)) g_DestroyParticle(mgr, it->second, true, true);
        it = g_matchFx.erase(it);
    }
}

// --- Item models changed in a match (a pick on the strategy screen) ---
// A match reads each model once, with the item the preset had then (the default weapon's
// file read as Twilight Schism's), and reads no file again while the model is in use. Nor
// does it take an item model asked for by its own name instead of the default one: the
// default one is then missing from the match's manifest (the error model). So a pick made
// in the match changes the model on the wearables themselves: the new item's model is
// loaded by its own name into the engine's just-in-time manifest (what the engine does for
// an asset that was not precached) and set on the wearable's skeleton, as the game sets
// the model of an item it equips. Only the strategy screen's model (a client-side entity,
// index from 16384) and your own hero's wearables (your illusions' too).
// A persona's models go on this way too (its swaps are never read into the files: the
// strategy screen's switch may take it off again), its body as well: the strategy screen's
// preview, and the hero you play made with its own model; the entities made once the game has
// started get them by name instead (see HookedFindOrRegister).
static std::map<std::string, void*> g_liveBindings;  // item model -> its binding (loaded once)
// the models loaded so in this match (any thread asks, see PersonaModelFor)
static std::set<std::string> g_liveResident;
static SRWLOCK g_liveResidentLock = SRWLOCK_INIT;

static void* LiveBinding(const std::string& model) {
    auto it = g_liveBindings.find(model);
    if (it != g_liveBindings.end()) return it->second;
    ResourceName name;
    void* binding = nullptr;
    if (g_SetResourceName(&name, model.c_str())) binding = g_BlockingLoad(g_ResourceSystem, &name, 0);
    auto status = (GetStatusFn)(*(void***)g_ResourceSystem)[kGetStatusSlot];
    bool resident = binding && status(g_ResourceSystem, &name) == kResourceResident;
    Log("[%c] Live model: %s loaded %s", resident ? '+' : '-', model.c_str(), resident ? "" : "(not resident)");
    if (!resident) binding = nullptr;
    g_liveBindings[model] = binding;
    if (resident) {
        AcquireSRWLockExclusive(&g_liveResidentLock);
        g_liveResident.insert(model);
        ReleaseSRWLockExclusive(&g_liveResidentLock);
    }
    return binding;
}

static bool IsLiveResident(const std::string& model) {
    AcquireSRWLockShared(&g_liveResidentLock);
    bool r = g_liveResident.count(model) != 0;
    ReleaseSRWLockShared(&g_liveResidentLock);
    return r;
}

// (see HookedFindOrRegister) The persona's model for one of the hero you play, when its
// persona is shown and both are loaded. Any thread.
static bool PersonaModelFor(const char* name, std::string& out) {
    if (!name) return false;
    size_t len = strlen(name);
    bool compiled = len > 7 && _stricmp(name + len - 7, ".vmdl_c") == 0;
    if (!compiled && (len < 6 || _stricmp(name + len - 5, ".vmdl") != 0)) return false;
    std::string base = Normalize(name);
    if (compiled) base.resize(base.size() - 2);
    std::string to;
    AcquireSRWLockShared(&g_rulesLock);
    const std::string* r = RuleFor(*g_rules, base);
    if (r && IsPersonaRule(*g_rules, base, r) && r->size() > 5 && r->compare(r->size() - 5, 5, ".vmdl") == 0) to = *r;
    ReleaseSRWLockShared(&g_rulesLock);
    if (to.empty() || !IsLiveResident(to) || !IsLiveResident(base)) return false;
    out = compiled ? to + "_c" : to;
    return true;
}

// Main thread, in a match: the models of the persona shown of the hero you play, and the
// ones they stand for, loaded into the engine's just-in-time manifest: then handed out by
// name (the original too, when the switch takes the persona off again)
static void PreloadPersonaModels() {
    if (g_inMenu || !g_SetResourceName || !g_BlockingLoad || !g_ResourceSystem) return;
    std::vector<std::pair<std::string, std::string>> pairs;
    AcquireSRWLockShared(&g_rulesLock);
    for (auto& r : g_rules->matchReplacements) {
        const std::string& dst = r.first;
        if (dst.size() < 6 || dst.compare(dst.size() - 5, 5, ".vmdl") != 0) continue;
        const std::string* to = RuleFor(*g_rules, dst);
        if (to && IsPersonaRule(*g_rules, dst, to) && to->size() > 5 && to->compare(to->size() - 5, 5, ".vmdl") == 0)
            pairs.push_back({dst, *to});
    }
    ReleaseSRWLockShared(&g_rulesLock);
    for (auto& pr : pairs) {
        LiveBinding(pr.first);
        LiveBinding(pr.second);
    }
    static size_t logged = (size_t)-1;
    if (pairs.size() != logged) {
        logged = pairs.size();
        Log("[*] Persona models for the match: %zu loaded with their originals", pairs.size());
    }
}

struct LiveSwap {
    const uint8_t* node;
    const uint8_t* ent;
    int index;
    uint32_t handle;
    std::string model, base, desired;
    bool wearable;
    int kind;  // 0 a model, 1 a body: a persona's put on live (or its own again)
};
// Bodies changed live (a persona's body on a hero made with its own, or its own again)
static std::set<uint32_t> g_bodyLive;
// The entities wearing a model put on live: entity handle -> {the model the game gave it (its
// swap is looked up by that one: the model put on may take its name's place), the one put on}
static std::unordered_map<uint32_t, std::pair<std::string, std::string>> g_liveOrig;

// Whether an entity is the hero you play or one of its illusions: an NPC of its unit name (a
// regular match gives its units no names: your hero's entity, and the NPCs it owns, then).
// Under g_rulesLock.
static bool IsLocalHeroNpc(const uint8_t* ent, int index) {
    static std::unordered_map<const void*, bool> npcs;  // (the interface thread's only)
    if (!ent) return false;
    if (g_heroIndex > 0 && index == g_heroIndex) return true;
    if (g_localNpc.empty() || !IsA(ent, ".?AVC_DOTA_BaseNPC@@", npcs)) return false;
    if (g_unitNameOffset > 0) {
        const char* unit = UnitNameOf(ent);
        if (unit && *unit) return g_localNpc == unit;
    }
    int owner = (int)(*(const uint32_t*)(ent + g_ih.ownerEntity) & 0x7fff);
    return g_heroIndex > 0 && owner == g_heroIndex;
}

// The model a hero's merged one is made of: "x_c_1.vmdl" -> "x.vmdl" (others as they are)
static std::string UncombinedName(const std::string& m) {
    if (m.size() < 10 || m.compare(m.size() - 5, 5, ".vmdl") != 0) return m;
    size_t dot = m.size() - 5, i = dot;
    while (i > 0 && isdigit((unsigned char)m[i - 1])) i--;
    if (i == dot || i < 3 || m.compare(i - 3, 3, "_c_") != 0) return m;
    return m.substr(0, i - 3) + ".vmdl";
}

static void LiveModelTick() {
    if (g_inMenu) {
        g_liveBindings.clear();  // the match's just-in-time manifest goes with it
        g_liveOrig.clear();
        g_bodyLive.clear();
        AcquireSRWLockExclusive(&g_liveResidentLock);
        g_liveResident.clear();
        ReleaseSRWLockExclusive(&g_liveResidentLock);
        return;
    }
    if (!g_itemHostsReady || !g_entityList || !g_SkeletonSetModel || !g_BlockingLoad || !g_SetResourceName || !g_ResourceSystem) return;
    std::vector<LiveSwap> todo;
    std::set<uint32_t> seen;
    std::vector<const uint8_t*> originals;  // bodies made with a persona's model: shown with their pieces
    std::vector<uint8_t*> unhide;           // pieces merged into a body put on live since
    static std::unordered_map<const void*, bool> npcs;  // (this thread's only)
    uint8_t** chunks = (uint8_t**)*g_entityList;
    std::set<std::string> noMerge;
    AcquireSRWLockShared(&g_personaLock);
    noMerge = g_noMerge;
    ReleaseSRWLockShared(&g_personaLock);
    AcquireSRWLockShared(&g_rulesLock);
    // the persona bodies handed out by name (the active persona swaps' models)
    std::set<std::string> personaModels;
    for (auto& r : g_rules->matchReplacements) {
        const std::string* to = RuleFor(*g_rules, r.first);
        if (to && IsPersonaRule(*g_rules, r.first, to)) personaModels.insert(*to);
    }
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
            seen.insert(handle);
            // the model its swap is looked up by: the one the game gave it
            std::string base = model;
            auto o = g_liveOrig.find(handle);
            if (o != g_liveOrig.end()) {
                if (model == o->second.first || model == o->second.second) base = o->second.first;
                else g_liveOrig.erase(o);  // the game gave it another model since
            }
            bool npc = IsA(ent, ".?AVC_DOTA_BaseNPC@@", npcs);
            // a hero in a match wears its body merged with its pieces (legion_commander_c_1):
            // its swap is the body's
            const std::string lookup = npc ? UncombinedName(base) : base;
            const std::string* to = RuleFor(*g_rules, lookup);
            bool persona = to && IsPersonaRule(*g_rules, lookup, to);
            // wearing a model of ours and no swap now (an item taken out, a persona switched
            // off): the game's again
            bool restore = !to && g_liveOrig.count(handle);
            const uint8_t* parentNode = *(const uint8_t* const*)(node + g_ih.parent);
            if (parentNode && !npc && g_hiddenByCombinerOffset > 0 && g_SetHiddenByCombiner &&
                *(const uint8_t*)(ent + g_hiddenByCombinerOffset)) {
                const uint8_t* holderEnt = *(const uint8_t* const*)(parentNode + g_ih.owner);
                const uint8_t* id = holderEnt ? *(const uint8_t* const*)(holderEnt + g_ih.entity) : nullptr;
                if (id && g_bodyLive.count(*(const uint32_t*)(id + 0x10))) unhide.push_back((uint8_t*)ent);
                // a piece of your hero not merged into its model (see g_noMerge)
                else if ((noMerge.count(model) || (to && noMerge.count(*to))) && IsLocalHeroNpc(holderEnt, EntityIndexOf(holderEnt)))
                    unhide.push_back((uint8_t*)ent);
            }
            if (g_heroIndex > 0 && index == g_heroIndex) {
                // what your hero has, when it changes (a persona's body put on or not)
                static std::string lastNote;
                std::string note = model + "|" + (to ? *to : std::string("-")) + (persona ? "|p" : "") +
                                   (parentNode ? "|parented" : "");
                if (note != lastNote) {
                    lastNote = note;
                    Log("[*] Your hero's model: %s (swap %s%s%s)", model.c_str(), to ? to->c_str() : "none",
                        persona ? ", the persona's" : "", parentNode ? ", parented" : "");
                }
            }
            // A hero's body (the strategy screen's preview, yours, your illusions'): a persona's
            // body put on live, and its own again when the persona goes off
            if (npc && (persona || (restore && g_bodyLive.count(handle)))) {
                if (index < 0x4000 && !IsLocalHeroNpc(ent, index)) continue;
                const std::string want = persona ? *to : base;
                if (want.size() < 6 || want.compare(want.size() - 5, 5, ".vmdl") != 0) continue;
                todo.push_back({node, ent, index, handle, model, base, want, false, 1});
                continue;
            }
            // one made with a persona's body (handed out by name in the game): said to be its
            // own model, or the game hides its pieces
            if (npc && personaModels.count(model) && (index >= 0x4000 || IsLocalHeroNpc(ent, index))) {
                originals.push_back(ent);
                continue;
            }
            if (!to && !restore) continue;
            const std::string desired = to ? *to : base;
            if (desired.size() < 5 || desired.compare(desired.size() - 5, 5, ".vmdl") != 0) continue;
            if (parentNode) {
                // a wearable: on a hero's bones (its scene node's parent): the strategy
                // screen's, yours or your illusions'
                const uint8_t* holderEnt = *(const uint8_t* const*)(parentNode + g_ih.owner);
                int holder = EntityIndexOf(holderEnt);
                if (holder < 0x4000 && !IsLocalHeroNpc(holderEnt, holder)) continue;
            } else {
                // a unit of its own (lane creeps, towers, couriers, wards): a general swap only.
                // Its model may be loaded from a match before the preset changed: the new one
                // goes on the units then, with no restart
                auto heroes = g_rules->heroes.find(base);
                if (heroes != g_rules->heroes.end() && !heroes->second.empty()) continue;
            }
            todo.push_back({node, ent, index, handle, model, base, desired, parentNode != nullptr, 0});
        }
    }
    ReleaseSRWLockShared(&g_rulesLock);
    for (auto it = g_liveOrig.begin(); it != g_liveOrig.end();) it = seen.count(it->first) ? std::next(it) : g_liveOrig.erase(it);
    for (auto it = g_bodyLive.begin(); it != g_bodyLive.end();) it = seen.count(*it) ? std::next(it) : g_bodyLive.erase(it);
    for (auto* w : unhide) g_SetHiddenByCombiner(w, false);
    if (g_isOriginalOffset > 0)
        for (auto* ent : originals)
            if (*(const uint8_t*)(ent + g_isOriginalOffset) != 1) *(uint8_t*)(ent + g_isOriginalOffset) = 1;
    for (auto& s : todo) {
        if (s.kind == 1) {
            // a body: the persona's (said to be its own model: the game shows its pieces on
            // it), or its own again (worked out by the game again)
            void* binding = LiveBinding(s.desired);
            if (!binding) continue;
            bool on = s.desired != s.base;
            if (on) g_liveOrig[s.handle] = {s.base, s.desired};
            else g_liveOrig.erase(s.handle);
            if (g_isOriginalOffset > 0) {
                uint8_t want = on ? 1 : 0;
                if (*(const uint8_t*)(s.ent + g_isOriginalOffset) != want) *(uint8_t*)(s.ent + g_isOriginalOffset) = want;
            }
            void* current = g_ih.modelHandle >= 0 ? *(void* const*)(s.node + g_ih.modelState + g_ih.modelHandle) : nullptr;
            if (current == binding) continue;
            g_SkeletonSetModel((void*)s.node, binding);
            if (on) g_bodyLive.insert(s.handle);
            else g_bodyLive.erase(s.handle);
            Log("[*] Live body: entity %d %s -> %s", s.index, s.model.c_str(), s.desired.c_str());
            continue;
        }
        std::string held = s.model;
        AcquireSRWLockShared(&g_loadedAsLock);
        auto f = g_loadedAs.find(s.model);
        if (f != g_loadedAs.end()) held = f->second;
        ReleaseSRWLockShared(&g_loadedAsLock);
        // (not "its file was read as that item already": that is the name's last read only.
        // A match reads a hero's models on its loading screen, as they are; a read of the
        // same name later, after the pick, as the item, left the hero's own model, read
        // before, as it was: The Barren Vector's claws stayed Sand King's own. The model
        // the entity holds decides; unknown, the last read does, as before)
        if (g_ih.modelHandle < 0 && held == s.desired) continue;
        void* binding = LiveBinding(s.desired);
        if (!binding) continue;
        // remembered while it wears a model of ours
        if (s.desired != s.base) g_liveOrig[s.handle] = {s.base, s.desired};
        else g_liveOrig.erase(s.handle);
        void* current = g_ih.modelHandle >= 0 ? *(void* const*)(s.node + g_ih.modelState + g_ih.modelHandle) : nullptr;
        if (current == binding) continue;
        g_SkeletonSetModel((void*)s.node, binding);
        // once per model and replacement (a wave of creeps is many units); a hero's
        // wearable once per entity
        static std::set<std::string> logged;
        std::string key = s.model + ">" + s.desired + (s.wearable ? "#" + std::to_string(s.index) : std::string());
        if (logged.insert(key).second)
            Log("[*] Live model: entity %d %s (read as %s) -> %s", s.index, s.model.c_str(), held.c_str(), s.desired.c_str());
    }
}

// The hero you picked (npc name) from the interface script: in this match the hero items'
// swaps apply to it only
static void SetLocalNpc(const char* v) {
    std::string npc = Normalize(v);
    while (!npc.empty() && (npc.back() == '\n' || npc.back() == '\r')) npc.pop_back();
    if (npc.compare(0, 14, "npc_dota_hero_") != 0) return;
    AcquireSRWLockExclusive(&g_rulesLock);
    bool changed = npc != g_localNpc;
    g_localNpc = npc;
    ReleaseSRWLockExclusive(&g_rulesLock);
    if (changed) {
        Log("[*] You play %s: its items' swaps apply to it only", npc.c_str());
        // its persona (the strategy screen's switch and preview): the loadout read again
        UpdatePersonaShown();
        InterlockedIncrement(&g_personaVersion);
    }
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
    // the scripts' messages come on the game's main thread
    if (!g_mainThread) g_mainThread = GetCurrentThreadId();
    // ticks and loop starts name their loop: "dash" (the menu's) or "hud" (the match's)
    int kind = strstr(p, " hud") ? kHud : kDash;
    if (strncmp(p, "DSL_TICK", 8) == 0) {
        WeatherTick();
        HudSkinTick();
        ShaderTick();
        if (g_inMenu) {
            RunPreloads();
            if (kind == kDash) PersonaTick(kDash);
            if (kind == kDash) MaybeRefreshPreview();
            MaybeSendGlows(kind);
        } else {
            MaybeTaunt();
            // the strategy screen's persona switch
            if (kind == kHud) PersonaTick(kHud);
        }
    }
    else if (strncmp(p, "DSL_HERO ", 9) == 0) {
        SetLocalHero(p + 9);
        if (!g_heroUnit.empty()) SetLocalNpc(g_heroUnit.c_str());
    }
    else if (strncmp(p, "DSL_PICKED ", 11) == 0) SetLocalNpc(p + 11);
    else if (strncmp(p, "DSL_PREGAME", 11) == 0) {
        LONG v = p[12] == '1';
        if (InterlockedExchange(&g_preGame, v) != v)
            Log("[*] %s", v ? "Before the game: a persona goes on the preview live" : "The game: a persona's models handed out by name");
    }
    else if (strncmp(p, "DSL_GTICK", 9) == 0) {
        MaybeSendGlows(kind);
        MaybeSendItemHosts(kind);
        RefreshItemHosts();
        LiveModelTick();
        MatchGlowTick();
        AttackFxTick();
        MatGroupTick();
        GemTick();
    }
    else if (strncmp(p, "DSL_LOOP", 8) == 0) {
        // a new script context: it has neither the glow table nor the item models yet
        if (g_injectPanel) {
            g_loopPanels[kind] = g_injectPanel;
            g_glowsSent[kind] = -1;
            g_itemHostsSent[kind] = "{}";
        }
    }
    else if (strncmp(p, "DSL_PICK", 8) == 0) {
        PersonaPicked(p[8] == ' ' ? p + 9 : nullptr);
        RecordPick(p[8] == ' ' ? p + 9 : nullptr);
    }
    else if (strncmp(p, "DSL_MENU", 8) == 0) {
        LONG v = p[9] == '1';
        if (InterlockedExchange(&g_inMenu, v) != v) {
            Log("[*] %s", v ? "In menu: renaming swapped models" : "In game: file swap only");
            // a new match: the hero you play is not known until you pick
            AcquireSRWLockExclusive(&g_rulesLock);
            g_localNpc.clear();
            ReleaseSRWLockExclusive(&g_rulesLock);
            InterlockedExchange(&g_localHeroId, 0);
            InterlockedExchange(&g_preGame, 1);
            // the emblem's attack effect: its binding taken again in the match
            g_attackBinding = nullptr;
            g_attackBound.clear();
            // the HUD skin: put on again for the new match
            g_hudShown = nullptr;
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
