/*
 * trem_audio.c - the port's FMOD + Steam Audio sound path (Windows).
 *
 * The same design as godot-steamaudio-dotnet (Steam Audio driving FMOD's
 * spatializer DSPs with the game's simulation), in C against the real
 * headers so the structure layouts cannot drift, and without .NET:
 *
 *   FMOD Core plays the game's .wav files from memory. Every voice has a
 *   "Steam Audio Spatializer" DSP (HRTF binaural rendering, occlusion and
 *   transmission through the map geometry, air absorption) fed by its own
 *   IPLSource, and a listener-centric "Steam Audio Reverb" DSP on the master
 *   group renders the map's simulated reflections (convolution reverb).
 *   Direct simulation runs every frame on the game thread; reflections run
 *   on a worker thread every ~100 ms.
 *
 * The DLLs (fmod.dll, phonon.dll, phonon_fmod.dll) are loaded from the
 * directory the game passes to ta_init; fmod.dll and phonon.dll are linked
 * delay-loaded, so a missing DLL only means ta_init fails and the game keeps
 * Godot's audio.
 *
 * Coordinates: callers pass Godot space (x right, y up, z back, metres).
 * Steam Audio uses that right-handed space directly; FMOD (initialised
 * left-handed, as phonon_fmod expects: it flips z itself) gets (x, y, -z).
 */
#include <windows.h>
#include <string.h>
#include <stdio.h>
#include <math.h>

#include "fmod.h"
#include "fmod_errors.h"
#include "phonon.h"

#define TA_VOICES 64
#define TA_SOUNDS 4096

/* phonon_fmod.dll's exports (steamaudio_fmod.h) */
typedef void (*PFN_iplFMODInitialize)(IPLContext);
typedef void (*PFN_iplFMODTerminate)(void);
typedef void (*PFN_iplFMODSetHRTF)(IPLHRTF);
typedef void (*PFN_iplFMODSetSimulationSettings)(IPLSimulationSettings);
typedef void (*PFN_iplFMODSetReverbSource)(IPLSource);
typedef IPLint32 (*PFN_iplFMODAddSource)(IPLSource);
typedef void (*PFN_iplFMODRemoveSource)(IPLint32);

/* IPLSpatializerParams (api-reference: DSP parameters, in order) */
enum {
    SPAT_SOURCE_POSITION = 0,
    SPAT_OVERALL_GAIN,
    SPAT_APPLY_DISTANCEATTENUATION,
    SPAT_APPLY_AIRABSORPTION,
    SPAT_APPLY_DIRECTIVITY,
    SPAT_APPLY_OCCLUSION,
    SPAT_APPLY_TRANSMISSION,
    SPAT_APPLY_REFLECTIONS,
    SPAT_APPLY_PATHING,
    SPAT_HRTF_INTERPOLATION,
    SPAT_DISTANCEATTENUATION,
    SPAT_DISTANCEATTENUATION_ROLLOFFTYPE,
    SPAT_DISTANCEATTENUATION_MINDISTANCE,
    SPAT_DISTANCEATTENUATION_MAXDISTANCE,
    SPAT_AIRABSORPTION_LOW,
    SPAT_AIRABSORPTION_MID,
    SPAT_AIRABSORPTION_HIGH,
    SPAT_DIRECTIVITY,
    SPAT_DIRECTIVITY_DIPOLEWEIGHT,
    SPAT_DIRECTIVITY_DIPOLEPOWER,
    SPAT_OCCLUSION,
    SPAT_TRANSMISSION_TYPE,
    SPAT_TRANSMISSION_LOW,
    SPAT_TRANSMISSION_MID,
    SPAT_TRANSMISSION_HIGH,
    SPAT_DIRECT_MIXLEVEL,
    SPAT_REFLECTIONS_BINAURAL,
    SPAT_REFLECTIONS_MIXLEVEL,
    SPAT_PATHING_BINAURAL,
    SPAT_PATHING_MIXLEVEL,
    SPAT_SIMULATION_OUTPUTS,
    SPAT_DIRECT_BINAURAL,
    SPAT_DISTANCE_ATTENUATION_RANGE,
    SPAT_SIMULATION_OUTPUTS_HANDLE,
    SPAT_OUTPUT_FORMAT,
    SPAT_NORMALIZE_PATHING_EQ
};
/* IPLReverbParams */
enum { REVERB_BINAURAL = 0, REVERB_OUTPUT_FORMAT };

typedef struct {
    FMOD_CHANNEL* ch;
    FMOD_DSP* dsp;
    IPLSource src;
    IPLint32 handle;
    float pos[3];
    int local;
    int active;
    unsigned gen;
    float occlusion; /* last value given to the spatializer (1 = clear) */
    int own;         /* the player's own placed sound: never occluded, no doppler */
    int big;         /* a loud sound (fire, impact, explosion): own reflections in enhanced mode */
    IPLSource xsrc;  /* reflections + pathing: in the simulator only while needed */
    int x_added;
    float prev[3], vel[3]; /* for doppler: last position, velocity (Godot units / s) */
    double prev_t;
    float base_freq, doppler;
    float thick;     /* metres of solid between it and the listener (ta_voice_thickness) */
    float trans_mid; /* the last transmission given (mid band), for ta_voice_info */
    float path_mix;  /* the last pathing mix level */
} Voice;

static void voice_direct(Voice* v, float occlusion, const float* transmission);
static double now_s(void);

/* The game's distance curve (snd_dma.c S_SpatializeOrigin, as the
 * spatializer's direct path uses it): full volume to 80 units, linear to
 * zero at 1330 (Godot units are 32 of them). The paths around walls use it
 * over their length, so a sound one corner away is as loud as an open one
 * as far away along the path (Steam Audio's default 1/r made them ~20x
 * quieter than the direct sound). */
static float IPLCALL game_falloff(IPLfloat32 d, void* unused) {
    (void)unused;
    const float lo = 80.0f / 32.0f, hi = 1330.0f / 32.0f;
    if (d <= lo) return 1.0f;
    if (d >= hi) return 0.0f;
    return (hi - d) / (hi - lo);
}
static void stop_pathing(int cancel);

/* tests: TREM_CRASH_LOG=<file> records access violations (first chance,
 * any thread, any module): the code, the faulting module + offset, and
 * the return addresses as module + offset (llvm-symbolizer with the PDB). */
static FILE* crash_log;
static DWORD S_thread_id(int which);
static volatile LONG crash_count;
static void crash_addr(FILE* f, const char* what, void* a) {
    HMODULE m = NULL;
    char name[MAX_PATH] = "?";
    if (GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, (LPCSTR)a, &m) && m) {
        GetModuleFileNameA(m, name, sizeof(name));
        const char* b = strrchr(name, '\\');
        fprintf(f, "%s %s+0x%llx\n", what, b ? b + 1 : name, (unsigned long long)((char*)a - (char*)m));
    } else {
        fprintf(f, "%s %p\n", what, a);
    }
}
static LONG CALLBACK crash_veh(EXCEPTION_POINTERS* e) {
    DWORD code = e->ExceptionRecord->ExceptionCode;
    if (!crash_log || (code != EXCEPTION_ACCESS_VIOLATION && code != EXCEPTION_STACK_OVERFLOW && code != EXCEPTION_ILLEGAL_INSTRUCTION && code != 0xC0000409))
        return EXCEPTION_CONTINUE_SEARCH;
    if (InterlockedIncrement(&crash_count) > 8) return EXCEPTION_CONTINUE_SEARCH;
    fprintf(crash_log, "exception %08lx thread %lu (reflection thread %lu, bake thread %lu) access %llu at %p\n", code, GetCurrentThreadId(),
            S_thread_id(0), S_thread_id(1), (unsigned long long)(e->ExceptionRecord->NumberParameters > 0 ? e->ExceptionRecord->ExceptionInformation[0] : 0),
            (void*)(e->ExceptionRecord->NumberParameters > 1 ? e->ExceptionRecord->ExceptionInformation[1] : 0));
    crash_addr(crash_log, "  at", e->ExceptionRecord->ExceptionAddress);
    void* fr[48];
    USHORT n = CaptureStackBackTrace(0, 48, fr, NULL);
    for (USHORT i = 0; i < n; i++) crash_addr(crash_log, "  frame", fr[i]);
    fflush(crash_log);
    return EXCEPTION_CONTINUE_SEARCH;
}

static FILE* phonon_log;
static void IPLCALL phonon_logcb(IPLLogLevel level, const char* message) {
    if (!phonon_log) return;
    static const char* names[] = {"info", "warning", "error", "debug"};
    fprintf(phonon_log, "%s: %s\n", (unsigned)level < 4 ? names[level] : "?", message);
    fflush(phonon_log);
}


static struct {
    int up;
    int enhanced;      /* ta_enhanced: limiter, doppler, reflections, pathing */
    FMOD_DSP* limiter; /* on the master bus, bypassed in vanilla mode */
    float lis_prev[3], lis_vel[3];
    double lis_t;
    /* sound pathing (enhanced): probes + baked paths per map, cached on disk */
    char path_cache[MAX_PATH * 2]; /* ta_pathing_cache: the file for the next scene */
    HANDLE bake_thread;
    IPLProbeBatch probes;          /* set (under lock) once baked or loaded */
    volatile LONG path_state;      /* 0 none, 1 baking, 2 ready, 3 failed */
    int num_probes;
    volatile LONG bake_stage;      /* the step the bake reached: 1 generate, 2 batch, 3 bake, 4 save, 5 done */
    volatile LONG bake_fault;      /* the step an access violation stopped (0 none) */
    volatile LONG bake_code;       /* its exception code (0xC0000005 access violation, 0xC00000FD stack overflow) */
    int pathing_on;    /* ta_features: apply the baked paths (s_pathing) */
    int hrtf_on;       /* ta_features: binaural (headphones) or panning (speakers) */
    int air_on;        /* ta_features: air absorption */
    int doppler_on;    /* ta_features: doppler (s_doppler) */
    int thick_on;      /* ta_features: wall thickness muffles transmission */
    int features;      /* the last ta_features bits (-1 before the first call) */
    /* CPU (ms): the direct simulation per update, the reflection / pathing
     * pass; averaged over ~1 s, and the quality level they set (0 full) */
    double direct_ms, direct_max, refl_ms, refl_max;
    double direct_acc; int direct_cnt;
    int quality;
    int refl_quality;  /* the worker's level: 0 full, 1, 2, 3 (fewer own-reflection sounds, shorter tails) */
    int refl_rays;
    int meter_voice;   /* ta_voice_meter: the voice whose output RMS is averaged (-1 none) */
    double meter_sum;
    int meter_n;
    float best_path[3]; /* ta_path_scan: the blocked probe with the strongest path */
    float far_visible[3]; /* ta_path_scan: the farthest probe in sight */
    int nospat; /* tests: TREM_FMOD_NOSPAT=1 plays every sound unspatialized */
    int nopath; /* tests: TREM_FMOD_NOPATH=1 bakes no pathing */
    int noreverb; /* tests: TREM_FMOD_NOREVERB=1 leaves the reverb send out */
    FMOD_CHANNEL* tone; /* tests: TREM_FMOD_TONE=1, 440 Hz at 0.5 for the first frames */
    int tone_frames;
    FILE* trace; /* tests: TREM_FMOD_TRACE=<file>, a line every 30 updates */
    unsigned updates;
    volatile LONG nan_blocks;   /* blocks in which a sanitiser replaced NaN/Inf */
    int last_voice;
    HMODULE m_fmod, m_phonon, m_plugin;
    FMOD_SYSTEM* sys;
    unsigned plugin;
    unsigned spat_plugin, reverb_plugin;
    FMOD_DSP* reverb_dsp;
    /* the voices play into `sfx`; a send of its mix feeds the reverb, whose
     * (wet only) output joins the master mix */
    FMOD_CHANNELGROUP* sfx;
    FMOD_DSPCONNECTION* reverb_send;
    unsigned plays, play_fails, loads, load_fails, direct_runs;
    float mesh_lo[3], mesh_hi[3]; /* acoustic mesh bounds, Godot space */
    FMOD_SOUND* sounds[TA_SOUNDS];
    int nsounds;
    Voice v[TA_VOICES];
    int next_voice;
    IPLContext ctx;
    IPLHRTF hrtf;
    IPLSimulator sim;
    IPLSimulationSettings simset;
    IPLScene scene;
    IPLStaticMesh mesh;
    IPLSource reverb_src;
    IPLint32 reverb_handle;
    float listener[12]; /* pos, fwd, up, right (Godot space) */
    int occlusion, reflections;
    float master;
    CRITICAL_SECTION lock;
    HANDLE thread;
    volatile LONG quit;
    volatile LONG have_scene;
    PFN_iplFMODInitialize fInit;
    PFN_iplFMODTerminate fTerm;
    PFN_iplFMODSetHRTF fSetHRTF;
    PFN_iplFMODSetSimulationSettings fSetSim;
    PFN_iplFMODSetReverbSource fSetReverb;
    PFN_iplFMODAddSource fAdd;
    PFN_iplFMODRemoveSource fRemove;
} S;

static DWORD S_thread_id(int which) {
    HANDLE h = which ? S.bake_thread : S.thread;
    return h ? GetThreadId(h) : 0;
}

static char g_err[512];

static int fail(const char* what, const char* detail) {
    _snprintf(g_err, sizeof(g_err) - 1, "%s%s%s", what, detail ? ": " : "", detail ? detail : "");
    return 0;
}

__declspec(dllexport) const char* ta_error(void) { return g_err; }

static HMODULE load_from(const wchar_t* dir, const wchar_t* name) {
    wchar_t path[MAX_PATH * 2];
    _snwprintf(path, MAX_PATH * 2 - 1, L"%s\\%s", dir, name);
    path[MAX_PATH * 2 - 1] = 0;
    return LoadLibraryExW(path, NULL, LOAD_WITH_ALTERED_SEARCH_PATH);
}

static FMOD_VECTOR fv(const float* g) {
    FMOD_VECTOR v;
    v.x = g[0];
    v.y = g[1];
    v.z = -g[2];
    return v;
}
static IPLVector3 iv(const float* g) {
    IPLVector3 v;
    v.x = g[0];
    v.y = g[1];
    v.z = g[2];
    return v;
}

static DWORD WINAPI reflection_thread(LPVOID unused) {
    (void)unused;
    while (!S.quit) {
        /* the budget: the worker rests at least three times as long as a
         * pass took (a pass of 30 ms runs every ~120 ms: a quarter of its
         * two threads), and never less than 90 ms */
        {
            DWORD rest = (DWORD)(S.refl_ms * 3.0);
            Sleep(rest > 90 ? (rest < 400 ? rest : 400) : 90);
        }
        if (S.quit) break;
        int path = S.enhanced && S.pathing_on && S.occlusion && S.path_state == 2;
        if (!S.have_scene || (!S.reflections && !path)) continue;
        EnterCriticalSection(&S.lock);
        double t0 = now_s();
        if (S.reflections) iplSimulatorRunReflections(S.sim);
        if (path) iplSimulatorRunPathing(S.sim);
        double ms = (now_s() - t0) * 1000.0;
        LeaveCriticalSection(&S.lock);
        S.refl_ms = S.refl_ms * 0.9 + ms * 0.1;
        if (ms > S.refl_max) S.refl_max = ms;
        /* the budget: a pass should leave the worker idle most of the time
         * (it runs every 90 ms on its own thread) */
        /* past the ray floor, cut the work that remains: the sounds with
         * their own reflections, then the length of every reverb tail */
        if (S.refl_ms > 30.0 && S.refl_rays <= 128 && S.refl_quality < 3) S.refl_quality++;
        else if (S.refl_ms < 10.0 && S.refl_quality > 0) S.refl_quality--;
        if (S.refl_ms > 25.0 && S.refl_rays > 128) S.refl_rays /= 2;
        else if (S.refl_ms < 8.0 && S.refl_rays < 1024) S.refl_rays *= 2;
    }
    return 0;
}

/* Find a nested plugin of phonon_fmod.dll by name. */
static unsigned find_plugin(const char* want) {
    int n = 0;
    if (FMOD_System_GetNumNestedPlugins(S.sys, S.plugin, &n) != FMOD_OK) return 0;
    for (int i = 0; i < n; i++) {
        unsigned h = 0;
        char name[128] = {0};
        FMOD_PLUGINTYPE type;
        unsigned ver = 0;
        if (FMOD_System_GetNestedPlugin(S.sys, S.plugin, i, &h) != FMOD_OK) continue;
        if (FMOD_System_GetPluginInfo(S.sys, h, &type, name, sizeof(name), &ver) != FMOD_OK) continue;
        if (strstr(name, want)) return h;
    }
    return 0;
}

/* Bring up FMOD + Steam Audio. `dir`: the folder with the three DLLs. */
__declspec(dllexport) int ta_init(const wchar_t* dir, int sample_rate) {
    FMOD_RESULT r;
    if (S.up) return 1;
    memset(&S, 0, sizeof(S));
    g_err[0] = 0;
    /* phonon_fmod.dll depends on phonon.dll: load it first, by full path */
    S.m_phonon = load_from(dir, L"phonon.dll");
    if (!S.m_phonon) return fail("phonon.dll not found", NULL);
    S.m_fmod = load_from(dir, L"fmod.dll");
    if (!S.m_fmod) return fail("fmod.dll not found", NULL);
    S.m_plugin = load_from(dir, L"phonon_fmod.dll");
    if (!S.m_plugin) return fail("phonon_fmod.dll not found", NULL);
    S.fInit = (PFN_iplFMODInitialize)GetProcAddress(S.m_plugin, "iplFMODInitialize");
    S.fTerm = (PFN_iplFMODTerminate)GetProcAddress(S.m_plugin, "iplFMODTerminate");
    S.fSetHRTF = (PFN_iplFMODSetHRTF)GetProcAddress(S.m_plugin, "iplFMODSetHRTF");
    S.fSetSim = (PFN_iplFMODSetSimulationSettings)GetProcAddress(S.m_plugin, "iplFMODSetSimulationSettings");
    S.fSetReverb = (PFN_iplFMODSetReverbSource)GetProcAddress(S.m_plugin, "iplFMODSetReverbSource");
    S.fAdd = (PFN_iplFMODAddSource)GetProcAddress(S.m_plugin, "iplFMODAddSource");
    S.fRemove = (PFN_iplFMODRemoveSource)GetProcAddress(S.m_plugin, "iplFMODRemoveSource");
    if (!S.fInit || !S.fSetHRTF || !S.fSetSim || !S.fAdd || !S.fRemove || !S.fSetReverb) return fail("phonon_fmod.dll lacks the iplFMOD* functions", NULL);

    /* FMOD */
    if ((r = FMOD_System_Create(&S.sys, FMOD_VERSION)) != FMOD_OK) return fail("FMOD_System_Create", FMOD_ErrorString(r));
    FMOD_System_SetSoftwareFormat(S.sys, sample_rate, FMOD_SPEAKERMODE_STEREO, 0);
    FMOD_System_SetDSPBufferSize(S.sys, 1024, 4);
    {
        char e[8];
        S.nospat = GetEnvironmentVariableA("TREM_FMOD_NOSPAT", e, sizeof(e)) > 0 && e[0] == '1';
        S.noreverb = GetEnvironmentVariableA("TREM_FMOD_NOREVERB", e, sizeof(e)) > 0 && e[0] == '1';
        S.nopath = GetEnvironmentVariableA("TREM_FMOD_NOPATH", e, sizeof(e)) > 0 && e[0] == '1';
        S.tone_frames = (GetEnvironmentVariableA("TREM_FMOD_TONE", e, sizeof(e)) > 0 && e[0] == '1') ? 90 : 0;
        char tp[MAX_PATH];
        if (GetEnvironmentVariableA("TREM_FMOD_TRACE", tp, sizeof(tp)) > 0) S.trace = fopen(tp, "w");
        char cp[MAX_PATH];
        if (!crash_log && GetEnvironmentVariableA("TREM_CRASH_LOG", cp, sizeof(cp)) > 0 && (crash_log = fopen(cp, "w")) != NULL)
            AddVectoredExceptionHandler(1, crash_veh);
    }
    /* tests: TREM_FMOD_WAV=<file> writes the mix to a .wav instead of the device */
    static char wav_path[MAX_PATH];
    void* extra = NULL;
    if (GetEnvironmentVariableA("TREM_FMOD_WAV", wav_path, sizeof(wav_path)) > 0) {
        FMOD_System_SetOutput(S.sys, FMOD_OUTPUTTYPE_WAVWRITER);
        extra = wav_path;
    }
    if ((r = FMOD_System_Init(S.sys, 256, FMOD_INIT_NORMAL, extra)) != FMOD_OK) return fail("FMOD_System_Init", FMOD_ErrorString(r));
    FMOD_System_Set3DSettings(S.sys, 1.0f, 1.0f, 1.0f);

    /* Steam Audio */
    IPLContextSettings cs;
    memset(&cs, 0, sizeof(cs));
    cs.version = STEAMAUDIO_VERSION;
    /* tests: TREM_PHONON_LOG=<file> turns on Steam Audio's API validation
     * layer and writes its messages there */
    {
        char lp[MAX_PATH];
        if (GetEnvironmentVariableA("TREM_PHONON_LOG", lp, sizeof(lp)) > 0 && (phonon_log = fopen(lp, "w")) != NULL) {
            cs.flags = IPL_CONTEXTFLAGS_VALIDATION;
            cs.logCallback = phonon_logcb;
        }
    }
    if (iplContextCreate(&cs, &S.ctx) != IPL_STATUS_SUCCESS) return fail("iplContextCreate", NULL);
    IPLAudioSettings as;
    as.samplingRate = sample_rate;
    as.frameSize = 1024;
    IPLHRTFSettings hs;
    memset(&hs, 0, sizeof(hs));
    hs.type = IPL_HRTFTYPE_DEFAULT;
    hs.volume = 1.0f;
    hs.normType = IPL_HRTFNORMTYPE_NONE;
    if (iplHRTFCreate(S.ctx, &as, &hs, &S.hrtf) != IPL_STATUS_SUCCESS) return fail("iplHRTFCreate", NULL);

    memset(&S.simset, 0, sizeof(S.simset));
    S.simset.flags = (IPLSimulationFlags)(IPL_SIMULATIONFLAGS_DIRECT | IPL_SIMULATIONFLAGS_REFLECTIONS | IPL_SIMULATIONFLAGS_PATHING);
    S.simset.sceneType = IPL_SCENETYPE_DEFAULT;
    S.simset.reflectionType = IPL_REFLECTIONEFFECTTYPE_CONVOLUTION;
    S.simset.maxNumOcclusionSamples = 16;
    S.simset.maxNumRays = 2048;
    S.simset.numDiffuseSamples = 32;
    S.simset.maxDuration = 1.5f;
    S.simset.maxOrder = 1;
    S.simset.maxNumSources = TA_VOICES + 2;
    S.simset.numThreads = 2;
    S.simset.rayBatchSize = 1;
    S.simset.numVisSamples = 4; /* pathing's run-time visibility checks (validation: must be > 0) */
    S.simset.samplingRate = sample_rate;
    S.simset.frameSize = 1024;
    if (iplSimulatorCreate(S.ctx, &S.simset, &S.sim) != IPL_STATUS_SUCCESS) return fail("iplSimulatorCreate", NULL);

    S.fInit(S.ctx);
    S.fSetHRTF(S.hrtf);
    S.fSetSim(S.simset);

    /* the plugin's effects */
    if ((r = FMOD_System_LoadPlugin(S.sys, "phonon_fmod.dll", &S.plugin, 0)) != FMOD_OK) {
        /* by full path when the search path misses it */
        char path[MAX_PATH * 2];
        WideCharToMultiByte(CP_UTF8, 0, dir, -1, path, sizeof(path) - 32, NULL, NULL);
        strcat(path, "\\phonon_fmod.dll");
        if ((r = FMOD_System_LoadPlugin(S.sys, path, &S.plugin, 0)) != FMOD_OK) return fail("FMOD_System_LoadPlugin phonon_fmod", FMOD_ErrorString(r));
    }
    S.spat_plugin = find_plugin("Spatializer");
    S.reverb_plugin = S.noreverb ? 0 : find_plugin("Reverb");
    if (!S.spat_plugin) return fail("no Steam Audio Spatializer in phonon_fmod.dll", NULL);

    if ((r = FMOD_System_CreateChannelGroup(S.sys, "sfx", &S.sfx)) != FMOD_OK) return fail("FMOD_System_CreateChannelGroup", FMOD_ErrorString(r));

    /* the listener-centric reverb */
    IPLSourceSettings ss;
    ss.flags = IPL_SIMULATIONFLAGS_REFLECTIONS;
    if (iplSourceCreate(S.sim, &ss, &S.reverb_src) == IPL_STATUS_SUCCESS) {
        iplSourceAdd(S.reverb_src, S.sim);
        S.fSetReverb(S.reverb_src);
        if (S.reverb_plugin && FMOD_System_CreateDSPByPlugin(S.sys, S.reverb_plugin, &S.reverb_dsp) == FMOD_OK) {
            /* the Reverb effect outputs only the reverb: it is a return fed by
             * a send, not an insert on the master (which would mute the mix) */
            FMOD_CHANNELGROUP* master = NULL;
            FMOD_DSP *master_head = NULL, *sfx_head = NULL;
            FMOD_System_GetMasterChannelGroup(S.sys, &master);
            FMOD_ChannelGroup_GetDSP(master, FMOD_CHANNELCONTROL_DSP_HEAD, &master_head);
            FMOD_ChannelGroup_GetDSP(S.sfx, FMOD_CHANNELCONTROL_DSP_HEAD, &sfx_head);
            FMOD_DSP_SetParameterBool(S.reverb_dsp, REVERB_BINAURAL, 1);
            FMOD_DSP_SetMeteringEnabled(S.reverb_dsp, 0, 1);
            if (master_head && sfx_head && FMOD_DSP_AddInput(master_head, S.reverb_dsp, NULL, FMOD_DSPCONNECTION_TYPE_STANDARD) == FMOD_OK &&
                FMOD_DSP_AddInput(S.reverb_dsp, sfx_head, &S.reverb_send, FMOD_DSPCONNECTION_TYPE_SEND) == FMOD_OK) {
                FMOD_DSP_SetActive(S.reverb_dsp, 1);
                FMOD_DSPConnection_SetMix(S.reverb_send, 0.6f);
            }
        }
    }

    /* the voices: one spatializer + one simulation source each */
    for (int i = 0; i < TA_VOICES; i++) {
        Voice* v = &S.v[i];
        /* the direct (occlusion) source is always simulated; the extra one
         * (reflections, pathing) only while its voice needs it: a source in
         * the simulator without inputs must not be simulated */
        ss.flags = IPL_SIMULATIONFLAGS_DIRECT;
        if (iplSourceCreate(S.sim, &ss, &v->src) != IPL_STATUS_SUCCESS) return fail("iplSourceCreate", NULL);
        iplSourceAdd(v->src, S.sim);
        ss.flags = (IPLSimulationFlags)(IPL_SIMULATIONFLAGS_REFLECTIONS | IPL_SIMULATIONFLAGS_PATHING);
        if (iplSourceCreate(S.sim, &ss, &v->xsrc) != IPL_STATUS_SUCCESS) return fail("iplSourceCreate", NULL);
        v->handle = S.fAdd(v->xsrc);
        if (FMOD_System_CreateDSPByPlugin(S.sys, S.spat_plugin, &v->dsp) != FMOD_OK) return fail("FMOD_System_CreateDSPByPlugin spatializer", NULL);
        FMOD_DSP_SetMeteringEnabled(v->dsp, 1, 1);
        FMOD_DSP_SetParameterInt(v->dsp, SPAT_SIMULATION_OUTPUTS_HANDLE, v->handle);
        /* Apply types (steamaudio_fmod.h ParameterApplyType): 0 off,
         * 1 simulation-defined, 2 user-defined. Distance and air absorption
         * "simulation-defined" are computed by the plugin itself; occlusion
         * and transmission would be read raw from the simulation outputs,
         * which are 0 (fully blocked) until the direct simulation has run
         * for the source - so they are user-defined here and fed by
         * ta_update (voice_direct), starting unblocked. */
        /* Steam Audio 4.8.1's spatializer defaults its direct mix level to
         * 0 (the direct sound muted, only reflections audible): set it. */
        FMOD_DSP_SetParameterFloat(v->dsp, SPAT_DIRECT_MIXLEVEL, 1.0f);
        FMOD_DSP_SetParameterInt(v->dsp, SPAT_APPLY_DISTANCEATTENUATION, 2);
        /* snd_dma.c S_SpatializeOrigin: full volume to SOUND_FULLVOLUME (80
         * units), then linear to zero at 80 + 1/SOUND_ATTENUATE (1330); in
         * Godot units (1/32) */
        FMOD_DSP_SetParameterInt(v->dsp, SPAT_DISTANCEATTENUATION_ROLLOFFTYPE, 1);
        FMOD_DSP_SetParameterFloat(v->dsp, SPAT_DISTANCEATTENUATION_MINDISTANCE, 80.0f / 32.0f);
        FMOD_DSP_SetParameterFloat(v->dsp, SPAT_DISTANCEATTENUATION_MAXDISTANCE, 1330.0f / 32.0f);
        FMOD_DSP_SetParameterInt(v->dsp, SPAT_APPLY_AIRABSORPTION, 0); /* ta_features */
        FMOD_DSP_SetParameterInt(v->dsp, SPAT_APPLY_OCCLUSION, 2);
        FMOD_DSP_SetParameterInt(v->dsp, SPAT_APPLY_TRANSMISSION, 2);
        voice_direct(v, 1.0f, NULL);
        FMOD_DSP_SetParameterBool(v->dsp, SPAT_APPLY_REFLECTIONS, 0);
        FMOD_DSP_SetParameterBool(v->dsp, SPAT_APPLY_PATHING, 0);
        FMOD_DSP_SetParameterBool(v->dsp, SPAT_PATHING_BINAURAL, 1);
        FMOD_DSP_SetParameterFloat(v->dsp, SPAT_PATHING_MIXLEVEL, 1.0f);
        FMOD_DSP_SetParameterFloat(v->dsp, SPAT_REFLECTIONS_MIXLEVEL, 0.8f);
        FMOD_DSP_SetParameterBool(v->dsp, SPAT_DIRECT_BINAURAL, 1);
        FMOD_DSP_SetParameterInt(v->dsp, SPAT_HRTF_INTERPOLATION, 1);
    }
    iplSimulatorCommit(S.sim);

    /* enhanced mode: a limiter on the master bus, so many loud sounds at
     * once (gunfire peaks at full scale) do not clip */
    {
        FMOD_CHANNELGROUP* master = NULL;
        if (FMOD_System_GetMasterChannelGroup(S.sys, &master) == FMOD_OK &&
            FMOD_System_CreateDSPByType(S.sys, FMOD_DSP_TYPE_LIMITER, &S.limiter) == FMOD_OK) {
            FMOD_DSP_SetParameterFloat(S.limiter, FMOD_DSP_LIMITER_RELEASETIME, 50.0f);
            FMOD_DSP_SetParameterFloat(S.limiter, FMOD_DSP_LIMITER_CEILING, -1.0f);
            FMOD_DSP_SetParameterFloat(S.limiter, FMOD_DSP_LIMITER_MAXIMIZERGAIN, 0.0f);
            FMOD_ChannelGroup_AddDSP(master, FMOD_CHANNELCONTROL_DSP_HEAD, S.limiter);
            FMOD_DSP_SetBypass(S.limiter, 1);
        }
    }

    S.occlusion = 1;
    S.reflections = 1;
    S.pathing_on = 1;
    S.hrtf_on = 1;
    S.air_on = 0;
    S.doppler_on = 1;
    S.features = -1;
    S.refl_rays = 1024;
    S.meter_voice = -1;
    S.master = 1.0f;
    S.listener[5] = -1.0f; /* forward -z */
    S.listener[7] = 1.0f;  /* up +y */
    S.listener[9] = 1.0f;  /* right +x */
    InitializeCriticalSection(&S.lock);
    S.thread = CreateThread(NULL, 0, reflection_thread, NULL, 0, NULL);
    S.up = 1;
    return 1;
}

__declspec(dllexport) void ta_shutdown(void) {
    if (!S.up) return;
    S.quit = 1;
    if (S.thread) {
        WaitForSingleObject(S.thread, 2000);
        CloseHandle(S.thread);
    }
    stop_pathing(1); /* quitting: do not wait out a bake */
    for (int i = 0; i < TA_VOICES; i++) {
        Voice* v = &S.v[i];
        if (v->ch) FMOD_Channel_Stop(v->ch);
        if (v->dsp) FMOD_DSP_Release(v->dsp);
        if (v->handle) S.fRemove(v->handle);
        if (v->src) {
            iplSourceRemove(v->src, S.sim);
            iplSourceRelease(&v->src);
        }
        if (v->xsrc) {
            if (v->x_added) iplSourceRemove(v->xsrc, S.sim);
            iplSourceRelease(&v->xsrc);
        }
    }
    if (S.reverb_dsp) FMOD_DSP_Release(S.reverb_dsp);
    for (int i = 0; i < S.nsounds; i++)
        if (S.sounds[i]) FMOD_Sound_Release(S.sounds[i]);
    FMOD_System_Release(S.sys);
    if (S.fTerm) S.fTerm();
    if (S.reverb_src) iplSourceRelease(&S.reverb_src);
    if (S.mesh) iplStaticMeshRelease(&S.mesh);
    if (S.scene) iplSceneRelease(&S.scene);
    if (S.sim) iplSimulatorRelease(&S.sim);
    if (S.hrtf) iplHRTFRelease(&S.hrtf);
    if (S.ctx) iplContextRelease(&S.ctx);
    DeleteCriticalSection(&S.lock);
    S.up = 0;
}

/* The map's geometry for occlusion, transmission and reflections (Godot
 * space). `mat`: per-triangle material index into the table below. */
/* The file the next scene's pathing probes are cached in (UTF-8). */
__declspec(dllexport) void ta_pathing_cache(const char* utf8_path) {
    if (!utf8_path) return;
    strncpy(S.path_cache, utf8_path, sizeof(S.path_cache) - 1);
    S.path_cache[sizeof(S.path_cache) - 1] = 0;
}

static IPLBakedDataIdentifier path_id(void) {
    IPLBakedDataIdentifier id;
    memset(&id, 0, sizeof(id));
    id.type = IPL_BAKEDDATATYPE_PATHING;
    id.variation = IPL_BAKEDDATAVARIATION_DYNAMIC;
    return id;
}

static int load_probes(IPLProbeBatch* out) {
    FILE* f = S.path_cache[0] ? fopen(S.path_cache, "rb") : NULL;
    if (!f) return 0;
    fseek(f, 0, SEEK_END);
    long n = ftell(f);
    fseek(f, 0, SEEK_SET);
    int ok = 0;
    IPLbyte* buf = n > 0 ? (IPLbyte*)malloc((size_t)n) : NULL;
    if (buf && fread(buf, 1, (size_t)n, f) == (size_t)n) {
        IPLSerializedObjectSettings so;
        memset(&so, 0, sizeof(so));
        so.data = buf;
        so.size = (IPLsize)n;
        IPLSerializedObject obj = NULL;
        if (iplSerializedObjectCreate(S.ctx, &so, &obj) == IPL_STATUS_SUCCESS) {
            ok = iplProbeBatchLoad(S.ctx, obj, out) == IPL_STATUS_SUCCESS;
            /* a loaded batch must be committed like a built one: without it
             * the simulator has no path simulator for it and
             * iplSimulatorRunPathing reads through a null pointer (found
             * with the cloud harness; it only shows once paths are looked up
             * for a real listener) */
            if (ok) iplProbeBatchCommit(*out);
            iplSerializedObjectRelease(&obj);
        }
    }
    free(buf);
    fclose(f);
    return ok;
}

static void save_probes(IPLProbeBatch batch) {
    if (!S.path_cache[0]) return;
    IPLSerializedObjectSettings so;
    memset(&so, 0, sizeof(so));
    IPLSerializedObject obj = NULL;
    if (iplSerializedObjectCreate(S.ctx, &so, &obj) != IPL_STATUS_SUCCESS) return;
    iplProbeBatchSave(batch, obj);
    FILE* f = fopen(S.path_cache, "wb");
    if (f) {
        fwrite(iplSerializedObjectGetData(obj), 1, (size_t)iplSerializedObjectGetSize(obj), f);
        fclose(f);
    }
    iplSerializedObjectRelease(&obj);
}

/* Steam Audio calls guarded by structured exception handling (MSVC): a fault
 * in a bake step disables pathing instead of taking the game down. */
typedef struct {
    IPLProbeArray arr;
    IPLProbeBatch batch;
    IPLProbeGenerationParams* gp;
    IPLPathBakeParams* bp;
} BakeCtx;

static void step_generate(BakeCtx* c) { iplProbeArrayGenerateProbes(c->arr, S.scene, c->gp); }
static void step_batch(BakeCtx* c) {
    iplProbeBatchAddProbeArray(c->batch, c->arr);
    iplProbeBatchCommit(c->batch);
}
/* the bake reads the scene: nothing else may simulate on it meanwhile (the
 * reflection thread and the direct simulation wait; sound plays on) */
static volatile LONG bake_locked;
/* Steam Audio 4.8.1 calls the progress callback without checking it for
 * NULL (a call to address 0): always pass one. It records the progress. */
static volatile LONG bake_permille;
static void IPLCALL bake_progress(IPLfloat32 f, void* unused) {
    (void)unused;
    InterlockedExchange(&bake_permille, (LONG)(f * 1000.0f));
}
static void step_bake(BakeCtx* c) {
    EnterCriticalSection(&S.lock);
    InterlockedExchange(&bake_locked, 1);
    iplPathBakerBake(S.ctx, c->bp, bake_progress, NULL);
    InterlockedExchange(&bake_locked, 0);
    LeaveCriticalSection(&S.lock);
}

static int guarded(void (*f)(BakeCtx*), BakeCtx* c, LONG stage) {
    InterlockedExchange(&S.bake_stage, stage);
#ifdef _MSC_VER
    __try {
        f(c);
    } __except (InterlockedExchange(&S.bake_code, (LONG)GetExceptionCode()), EXCEPTION_EXECUTE_HANDLER) {
        InterlockedExchange(&S.bake_fault, stage);
        if (InterlockedExchange(&bake_locked, 0)) LeaveCriticalSection(&S.lock);
        return 0;
    }
#else
    f(c);
#endif
    return 1;
}

/* Probes ~1.5 m above every floor, 3 m apart, and the paths between them:
 * baked once per map in the background, then cached. */
static DWORD WINAPI bake_thread(LPVOID unused) {
    (void)unused;
    IPLProbeBatch batch = NULL;
    int ok = load_probes(&batch);
    if (!ok) {
        IPLProbeArray arr = NULL;
        if (iplProbeArrayCreate(S.ctx, &arr) == IPL_STATUS_SUCCESS) {
            IPLProbeGenerationParams gp;
            memset(&gp, 0, sizeof(gp));
            gp.type = IPL_PROBEGENERATIONTYPE_UNIFORMFLOOR;
            gp.spacing = 3.0f;
            gp.height = 1.5f;
            /* the volume over the mesh bounds: Steam Audio's unit cube is
             * centred on the origin (not 0..1 as phonon.h says), so scale by
             * the size and translate to the centre (row-major) */
            for (int k = 0; k < 3; k++) {
                gp.transform.elements[k][k] = S.mesh_hi[k] - S.mesh_lo[k];
                gp.transform.elements[k][3] = 0.5f * (S.mesh_lo[k] + S.mesh_hi[k]);
            }
            gp.transform.elements[3][3] = 1.0f;
            BakeCtx c;
            memset(&c, 0, sizeof(c));
            c.arr = arr;
            c.gp = &gp;
            int gen_ok = guarded(step_generate, &c, 1);
            S.num_probes = gen_ok ? iplProbeArrayGetNumProbes(arr) : 0;
            if (gen_ok && S.num_probes > 0 && iplProbeBatchCreate(S.ctx, &batch) == IPL_STATUS_SUCCESS && (c.batch = batch, guarded(step_batch, &c, 2))) {
                IPLPathBakeParams bp;
                memset(&bp, 0, sizeof(bp));
                bp.scene = S.scene;
                bp.probeBatch = batch;
                bp.identifier = path_id();
                bp.numSamples = 1;
                bp.radius = 0.5f;
                bp.threshold = 0.1f;
                bp.visRange = 50.0f;
                bp.pathRange = 100.0f;
                bp.numThreads = 2;
                c.bp = &bp;
                ok = guarded(step_bake, &c, 3) && S.path_state == 1; /* not faulted, not cancelled */
                if (ok) {
                    InterlockedExchange(&S.bake_stage, 4);
                    save_probes(batch);
                    InterlockedExchange(&S.bake_stage, 5);
                }
            }
            iplProbeArrayRelease(&arr);
        }
    }
    if (ok && S.path_state == 1) {
        EnterCriticalSection(&S.lock);
        S.probes = batch;
        S.num_probes = iplProbeBatchGetNumProbes(batch);
        iplSimulatorAddProbeBatch(S.sim, batch);
        iplSimulatorCommit(S.sim);
        InterlockedExchange(&S.path_state, 2);
        LeaveCriticalSection(&S.lock);
    } else {
        if (batch) iplProbeBatchRelease(&batch);
        if (S.path_state == 1) InterlockedExchange(&S.path_state, 3);
    }
    return 0;
}

/* `cancel` 0 lets a bake in progress run to its end (its probes are dropped):
 * Steam Audio 4.8.1 crashes after iplPathBakerCancelBake, a thread of
 * phonon.dll's own reading memory the cancel freed (a map change during
 * transit's first bake: an access violation in phonon.dll, every time, with
 * the scene, probes and bake parameters all kept alive). Waiting costs the
 * rest of the bake once; the next visit loads the cached probes. */
static void stop_pathing(int cancel) {
    if (S.bake_thread) {
        InterlockedExchange(&S.path_state, 0);
        if (cancel) iplPathBakerCancelBake(S.ctx);
        WaitForSingleObject(S.bake_thread, INFINITE);
        CloseHandle(S.bake_thread);
        S.bake_thread = NULL;
    }
    EnterCriticalSection(&S.lock);
    if (S.probes) {
        iplSimulatorRemoveProbeBatch(S.sim, S.probes);
        iplSimulatorCommit(S.sim);
        iplProbeBatchRelease(&S.probes);
    }
    S.num_probes = 0;
    InterlockedExchange(&S.path_state, 0);
    LeaveCriticalSection(&S.lock);
}

/* Acoustic materials. A triangle's index is vanilla * 8 + enhanced: the
 * vanilla class keeps the original three-material table (generic, metal,
 * rock / organic), the enhanced class one of eight (Steam Audio's preset
 * values; organic is soft and dead, like heavy fabric). ta_enhanced swaps
 * the table the scene uses. */
#define TA_MATS 24
static const IPLMaterial vanilla_mats[3] = {
    {{0.10f, 0.20f, 0.30f}, 0.05f, {0.100f, 0.050f, 0.030f}}, /* generic */
    {{0.20f, 0.07f, 0.06f}, 0.05f, {0.200f, 0.025f, 0.010f}}, /* metal */
    {{0.13f, 0.20f, 0.24f}, 0.10f, {0.015f, 0.002f, 0.001f}}, /* rock / organic */
};
static const IPLMaterial enhanced_mats[8] = {
    {{0.10f, 0.20f, 0.30f}, 0.05f, {0.100f, 0.050f, 0.030f}}, /* 0 generic */
    {{0.20f, 0.07f, 0.06f}, 0.05f, {0.200f, 0.025f, 0.010f}}, /* 1 metal: rings */
    {{0.13f, 0.20f, 0.24f}, 0.05f, {0.015f, 0.002f, 0.001f}}, /* 2 rock */
    {{0.05f, 0.07f, 0.08f}, 0.05f, {0.015f, 0.002f, 0.001f}}, /* 3 concrete: hard, reflective */
    {{0.30f, 0.60f, 0.75f}, 0.30f, {0.050f, 0.010f, 0.005f}}, /* 4 organic (creep, flesh, hive): dead */
    {{0.06f, 0.03f, 0.02f}, 0.05f, {0.060f, 0.044f, 0.011f}}, /* 5 glass */
    {{0.60f, 0.70f, 0.80f}, 0.05f, {0.031f, 0.012f, 0.008f}}, /* 6 gravel / dirt */
    {{0.11f, 0.07f, 0.06f}, 0.05f, {0.070f, 0.014f, 0.005f}}, /* 7 wood */
};
static int scene_mats_enhanced = -1;
static void scene_materials(int enhanced) {
    if (!S.mesh || scene_mats_enhanced == enhanced) return;
    scene_mats_enhanced = enhanced;
    for (int i = 0; i < TA_MATS; i++) {
        IPLMaterial m = enhanced ? enhanced_mats[i % 8] : vanilla_mats[i / 8];
        iplStaticMeshSetMaterial(S.mesh, S.scene, &m, i);
    }
    iplSceneCommit(S.scene);
}

__declspec(dllexport) int ta_set_scene(const float* verts, int nverts, const int* tris, int ntris, const int* mat) {
    IPLMaterial materials[TA_MATS];
    for (int i = 0; i < TA_MATS; i++) materials[i] = S.enhanced ? enhanced_mats[i % 8] : vanilla_mats[i / 8];
    if (!S.up) return 0;
    stop_pathing(0); /* the old scene's probes and any bake in progress */
    EnterCriticalSection(&S.lock);
    InterlockedExchange(&S.have_scene, 0);
    if (S.mesh) {
        iplStaticMeshRemove(S.mesh, S.scene);
        iplStaticMeshRelease(&S.mesh);
    }
    if (S.scene) iplSceneRelease(&S.scene);
    IPLSceneSettings sc;
    memset(&sc, 0, sizeof(sc));
    sc.type = IPL_SCENETYPE_DEFAULT;
    int ok = iplSceneCreate(S.ctx, &sc, &S.scene) == IPL_STATUS_SUCCESS;
    for (int k = 0; k < 3; k++) {
        S.mesh_lo[k] = 1e30f;
        S.mesh_hi[k] = -1e30f;
    }
    for (int i = 0; i < nverts; i++)
        for (int k = 0; k < 3; k++) {
            float c = verts[i * 3 + k];
            if (c < S.mesh_lo[k]) S.mesh_lo[k] = c;
            if (c > S.mesh_hi[k]) S.mesh_hi[k] = c;
        }
    if (ok && ntris > 0) {
        IPLStaticMeshSettings ms;
        ms.numVertices = nverts;
        ms.numTriangles = ntris;
        ms.numMaterials = TA_MATS;
        ms.vertices = (IPLVector3*)verts;
        ms.triangles = (IPLTriangle*)tris;
        ms.materialIndices = (IPLint32*)mat;
        ms.materials = materials;
        ok = iplStaticMeshCreate(S.scene, &ms, &S.mesh) == IPL_STATUS_SUCCESS;
        if (ok) {
            iplStaticMeshAdd(S.mesh, S.scene);
            iplSceneCommit(S.scene);
            scene_mats_enhanced = S.enhanced;
        }
    }
    if (ok) {
        iplSimulatorSetScene(S.sim, S.scene);
        iplSimulatorCommit(S.sim);
        InterlockedExchange(&S.have_scene, 1);
    }
    LeaveCriticalSection(&S.lock);
    if (!ok) fail("Steam Audio scene creation failed", NULL);
    /* pathing: s_pathing (no cache file = off) */
    if (ok && ntris > 0 && !S.nopath && S.path_cache[0]) {
        InterlockedExchange(&S.path_state, 1);
        /* a large stack: the visibility graph build recurses deeply */
        S.bake_thread = CreateThread(NULL, 64u << 20, bake_thread, NULL, STACK_SIZE_PARAM_IS_A_RESERVATION, NULL);
    }
    return ok;
}

/* A sound from a .wav / .ogg file's bytes; -1 on failure. */
__declspec(dllexport) int ta_sound_load(const void* data, int len) {
    if (!S.up || S.nsounds >= TA_SOUNDS) return -1;
    FMOD_CREATESOUNDEXINFO ex;
    memset(&ex, 0, sizeof(ex));
    ex.cbsize = sizeof(ex);
    ex.length = (unsigned)len;
    FMOD_SOUND* s = NULL;
    FMOD_RESULT r = FMOD_System_CreateSound(S.sys, (const char*)data, FMOD_OPENMEMORY | FMOD_CREATESAMPLE | FMOD_2D | FMOD_LOOP_OFF, &ex, &s);
    if (r != FMOD_OK) {
        fail("FMOD_System_CreateSound", FMOD_ErrorString(r));
        S.load_fails++;
        return -1;
    }
    S.loads++;
    S.sounds[S.nsounds] = s;
    return S.nsounds++;
}

static int voice_playing(Voice* v) {
    FMOD_BOOL p = 0;
    if (!v->ch) return 0;
    if (FMOD_Channel_IsPlaying(v->ch, &p) != FMOD_OK) return 0;
    return p ? 1 : 0;
}

static void set_source_position(Voice* v) {
    FMOD_DSP_PARAMETER_3DATTRIBUTES a;
    memset(&a, 0, sizeof(a));
    a.absolute.position = fv(v->pos);
    float fwd[3] = {0.0f, 0.0f, -1.0f}, up[3] = {0.0f, 1.0f, 0.0f};
    a.absolute.forward = fv(fwd);
    a.absolute.up = fv(up);
    /* relative to the listener (FMOD's own convention, left-handed) */
    float d[3] = {v->pos[0] - S.listener[0], v->pos[1] - S.listener[1], v->pos[2] - S.listener[2]};
    float* lf = &S.listener[3];
    float* lu = &S.listener[6];
    float* lr = &S.listener[9];
    a.relative.position.x = d[0] * lr[0] + d[1] * lr[1] + d[2] * lr[2];
    a.relative.position.y = d[0] * lu[0] + d[1] * lu[1] + d[2] * lu[2];
    a.relative.position.z = d[0] * lf[0] + d[1] * lf[1] + d[2] * lf[2];
    a.relative.forward.z = 1.0f;
    a.relative.up.y = 1.0f;
    FMOD_DSP_SetParameterData(v->dsp, SPAT_SOURCE_POSITION, &a, sizeof(a));
}

/* An effect whose output meter reads NaN / Inf has a poisoned internal
 * state (HRTF filters, convolution reverb): every later mixed sample would be
 * NaN and the output silent for good. Reset it; returns 1 if it was. */
static int unpoison(FMOD_DSP* dsp) {
    if (!dsp) return 0;
    FMOD_DSP_METERING_INFO out;
    memset(&out, 0, sizeof(out));
    if (FMOD_DSP_GetMeteringInfo(dsp, NULL, &out) != FMOD_OK) return 0;
    for (int c = 0; c < out.numchannels && c < 32; c++) {
        if (!isfinite(out.peaklevel[c]) || !isfinite(out.rmslevel[c])) {
            FMOD_DSP_Reset(dsp);
            InterlockedIncrement(&S.nan_blocks);
            return 1;
        }
    }
    return 0;
}

/* Occlusion and transmission for a voice (user-defined apply type). */
static double now_s(void) {
    LARGE_INTEGER f, c;
    QueryPerformanceFrequency(&f);
    QueryPerformanceCounter(&c);
    return (double)c.QuadPart / (double)f.QuadPart;
}

/* Velocity from successive positions (smoothed, clamped to 60 units/s). */
static void track(float* prev, float* vel, double* prev_t, const float* pos) {
    double t = now_s();
    double dt = t - *prev_t;
    if (*prev_t > 0 && dt > 0.004 && dt < 0.5) {
        for (int k = 0; k < 3; k++) {
            float v = (float)((pos[k] - prev[k]) / dt);
            if (v > 60.0f) v = 60.0f;
            if (v < -60.0f) v = -60.0f;
            vel[k] = vel[k] * 0.5f + v * 0.5f;
        }
    } else if (dt >= 0.5) {
        vel[0] = vel[1] = vel[2] = 0.0f;
    }
    memcpy(prev, pos, sizeof(float) * 3);
    *prev_t = t;
}

static void voice_direct(Voice* v, float occlusion, const float* transmission) {
    float t1[3] = {1.0f, 1.0f, 1.0f};
    const float* t = transmission ? transmission : t1;
    float tt[3];
    if (S.thick_on && v->thick > 0.0f && transmission) {
        /* through solid: low frequencies pass a thin wall, little passes a
         * thick one (per metre of solid along the line) */
        /* per metre: a 0.25 m door keeps 84 / 70 / 50 %, a 0.66 m wall
         * 63 / 40 / 16 %, 2 m of rock 25 / 6 / 0.4 % (low / mid / high) */
        static const float k[3] = {0.7f, 1.4f, 2.8f};
        for (int b = 0; b < 3; b++) tt[b] = t[b] * expf(-v->thick * k[b]);
        t = tt;
    }
    v->trans_mid = t[1];
    v->occlusion = occlusion;
    FMOD_DSP_SetParameterFloat(v->dsp, SPAT_OCCLUSION, occlusion);
    FMOD_DSP_SetParameterFloat(v->dsp, SPAT_TRANSMISSION_LOW, t[0]);
    FMOD_DSP_SetParameterFloat(v->dsp, SPAT_TRANSMISSION_MID, t[1]);
    FMOD_DSP_SetParameterFloat(v->dsp, SPAT_TRANSMISSION_HIGH, t[2]);
}

/* Play sound `id`; `local`: the listener's own sound (no spatialization).
 * Returns a voice id (index | generation << 8) or -1. */
/* local: 0 positioned, 1 unspatialized (vanilla local sounds), 2 the
 * player's own positioned sound (never occluded, no doppler). */
__declspec(dllexport) int ta_play(int id, const float* pos, int local, int loop, float volume) {
    if (!S.up || id < 0 || id >= S.nsounds) return -1;
    int big = (local & 4) != 0;
    int own = (local & 3) == 2;
    local = (local & 3) == 1;
    int best = -1;
    for (int k = 0; k < TA_VOICES; k++) {
        int i = (S.next_voice + k) % TA_VOICES;
        if (!voice_playing(&S.v[i])) {
            best = i;
            break;
        }
    }
    if (best < 0) {
        /* steal the oldest non-looping voice */
        best = S.next_voice;
        FMOD_Channel_Stop(S.v[best].ch);
    }
    S.next_voice = (best + 1) % TA_VOICES;
    Voice* v = &S.v[best];
    if (v->ch) {
        FMOD_Channel_RemoveDSP(v->ch, v->dsp);
    }
    /* a fresh start for the reused spatializer (no tail from the last sound) */
    FMOD_DSP_DisconnectAll(v->dsp, 1, 1);
    FMOD_DSP_Reset(v->dsp);
    voice_direct(v, 1.0f, NULL);
    FMOD_CHANNEL* ch = NULL;
    FMOD_RESULT pr = FMOD_System_PlaySound(S.sys, S.sounds[id], S.sfx, 1, &ch);
    if (pr != FMOD_OK) {
        fail("FMOD_System_PlaySound", FMOD_ErrorString(pr));
        S.play_fails++;
        return -1;
    }
    S.plays++;
    FMOD_Channel_SetMode(ch, loop ? FMOD_LOOP_NORMAL : FMOD_LOOP_OFF);
    FMOD_Channel_SetVolume(ch, volume);
    v->ch = ch;
    v->local = local;
    v->own = own;
    v->big = big;
    /* enhanced: pathing for every positioned sound once the map is baked,
     * per-source reflections for the loud ones */
    FMOD_DSP_SetParameterBool(v->dsp, SPAT_APPLY_PATHING, 0);
    FMOD_DSP_SetParameterBool(v->dsp, SPAT_APPLY_REFLECTIONS, 0);
    v->base_freq = 0.0f;
    v->doppler = 1.0f;
    v->thick = 0.0f;
    v->path_mix = 0.0f;
    FMOD_Channel_GetFrequency(ch, &v->base_freq);
    v->vel[0] = v->vel[1] = v->vel[2] = 0.0f;
    v->prev_t = 0;
    if (pos) track(v->prev, v->vel, &v->prev_t, pos);
    v->active = 1;
    v->gen = (v->gen + 1) & 0x7fffff;
    if (pos) memcpy(v->pos, pos, sizeof(v->pos));
    if (!local && pos) {
        float dx = pos[0] - S.listener[0], dy = pos[1] - S.listener[1], dz = pos[2] - S.listener[2];
        /* within 2 units (1/16 m) of the listener: as vanilla plays the
         * listener's own sounds, unspatialized (and no zero-length direction
         * reaches the HRTF) */
        if (dx * dx + dy * dy + dz * dz < (2.0f / 32.0f) * (2.0f / 32.0f)) v->local = local = 1;
    }
    if (!local && !S.nospat) {
        set_source_position(v);
        FMOD_Channel_AddDSP(ch, 0, v->dsp);
    }
    FMOD_Channel_SetPaused(ch, 0);
    S.last_voice = best;
    return best | (int)(v->gen << 8);
}

static Voice* voice_of(int h) {
    if (h < 0) return NULL;
    Voice* v = &S.v[h & 0xff];
    if ((h & 0xff) >= TA_VOICES || v->gen != (unsigned)(h >> 8)) return NULL;
    return v;
}

__declspec(dllexport) void ta_voice_set_pos(int h, const float* pos) {
    Voice* v = voice_of(h);
    if (!S.up || !v) return;
    memcpy(v->pos, pos, sizeof(v->pos));
    track(v->prev, v->vel, &v->prev_t, pos);
}

__declspec(dllexport) void ta_voice_stop(int h) {
    Voice* v = voice_of(h);
    if (!S.up || !v || !v->ch) return;
    FMOD_Channel_Stop(v->ch);
    v->active = 0;
}

__declspec(dllexport) int ta_voice_playing(int h) {
    Voice* v = voice_of(h);
    return (S.up && v) ? voice_playing(v) : 0;
}

/* The listener (Godot space): position, forward, up. */
__declspec(dllexport) void ta_listener(const float* pos, const float* fwd, const float* up) {
    if (!S.up) return;
    memcpy(&S.listener[0], pos, 12);
    track(S.lis_prev, S.lis_vel, &S.lis_t, pos);
    memcpy(&S.listener[3], fwd, 12);
    memcpy(&S.listener[6], up, 12);
    /* right = fwd x up */
    S.listener[9] = fwd[1] * up[2] - fwd[2] * up[1];
    S.listener[10] = fwd[2] * up[0] - fwd[0] * up[2];
    S.listener[11] = fwd[0] * up[1] - fwd[1] * up[0];
    FMOD_VECTOR p = fv(pos), f = fv(fwd), u = fv(up), vel = {0.0f, 0.0f, 0.0f};
    FMOD_System_Set3DListenerAttributes(S.sys, 0, &p, &vel, &f, &u);
}

__declspec(dllexport) void ta_enhanced(int on) {
    if (!S.up) return;
    S.enhanced = on;
    if (S.limiter) FMOD_DSP_SetBypass(S.limiter, !on);
    /* the scene's material table follows in ta_update (under the lock,
     * which a bake may hold for seconds) */
}

/* Doppler (enhanced): pitch from the source and listener velocities along
 * the line between them; c = 343 m/s, Godot units are ~1 m. */
static void doppler(void) {
    for (int i = 0; i < TA_VOICES; i++) {
        Voice* v = &S.v[i];
        if (!v->active || !v->ch || v->base_freq <= 0.0f) continue;
        float f = 1.0f;
        /* a sound no longer moved has stopped (its last speed must not
         * keep its pitch shifted) */
        if (v->prev_t > 0 && now_s() - v->prev_t > 0.15) v->vel[0] = v->vel[1] = v->vel[2] = 0.0f;
        if (S.enhanced && S.doppler_on && !v->local && !v->own) {
            float u[3] = {S.listener[0] - v->pos[0], S.listener[1] - v->pos[1], S.listener[2] - v->pos[2]};
            float d = sqrtf(u[0] * u[0] + u[1] * u[1] + u[2] * u[2]);
            if (d > 0.05f) {
                float vs = (v->vel[0] * u[0] + v->vel[1] * u[1] + v->vel[2] * u[2]) / d;
                float vl = (S.lis_vel[0] * u[0] + S.lis_vel[1] * u[1] + S.lis_vel[2] * u[2]) / d;
                f = (343.0f - vl) / (343.0f - vs);
                if (f < 0.5f) f = 0.5f;
                if (f > 2.0f) f = 2.0f;
            }
        }
        if (fabsf(f - v->doppler) > 0.001f) {
            v->doppler = f;
            FMOD_Channel_SetFrequency(v->ch, v->base_freq * f);
        }
    }
}

__declspec(dllexport) void ta_options(int occlusion, int reflections, float master) {
    if (!S.up) return;
    S.occlusion = occlusion;
    S.reflections = reflections;
    S.master = master;
    FMOD_CHANNELGROUP* mg = NULL;
    if (FMOD_System_GetMasterChannelGroup(S.sys, &mg) == FMOD_OK) FMOD_ChannelGroup_SetVolume(mg, master);
    for (int i = 0; i < TA_VOICES; i++) {
        if (!S.v[i].dsp) continue;
        FMOD_DSP_SetParameterInt(S.v[i].dsp, SPAT_APPLY_OCCLUSION, occlusion ? 2 : 0);
        FMOD_DSP_SetParameterInt(S.v[i].dsp, SPAT_APPLY_TRANSMISSION, occlusion ? 2 : 0);
    }
    if (S.reverb_send) FMOD_DSPConnection_SetMix(S.reverb_send, reflections ? 0.6f : 0.0f);
}

/* Counters and the output's state for `s_fmodinfo`. */
__declspec(dllexport) void ta_stats(char* buf, int len) {
    int playing = 0;
    for (int i = 0; i < TA_VOICES; i++) playing += voice_playing(&S.v[i]);
    float vol = -1.0f;
    FMOD_CHANNELGROUP* mg = NULL;
    if (S.up && FMOD_System_GetMasterChannelGroup(S.sys, &mg) == FMOD_OK) FMOD_ChannelGroup_GetVolume(mg, &vol);
    int drivers = 0;
    if (S.up) FMOD_System_GetNumDrivers(S.sys, &drivers);
    /* mean occlusion of the spatialized voices, and the master bus peak */
    float occ = 0.0f;
    int nocc = 0;
    for (int i = 0; i < TA_VOICES; i++) {
        if (S.v[i].active && !S.v[i].local && voice_playing(&S.v[i])) {
            occ += S.v[i].occlusion;
            nocc++;
        }
    }
    float peak = -1.0f;
    FMOD_DSP* head = NULL;
    if (mg && FMOD_ChannelGroup_GetDSP(mg, FMOD_CHANNELCONTROL_DSP_HEAD, &head) == FMOD_OK) {
        FMOD_DSP_METERING_INFO out;
        memset(&out, 0, sizeof(out));
        FMOD_DSP_SetMeteringEnabled(head, 0, 1);
        if (FMOD_DSP_GetMeteringInfo(head, NULL, &out) == FMOD_OK && out.numchannels > 0) {
            peak = 0.0f;
            for (int c = 0; c < out.numchannels; c++) peak = out.peaklevel[c] > peak ? out.peaklevel[c] : peak;
        }
    }
    const float* sp = NULL;
    for (int i = 0; i < TA_VOICES && !sp; i++)
        if (S.v[i].active && !S.v[i].local && voice_playing(&S.v[i])) sp = S.v[i].pos;
    float none[3] = {0.0f, 0.0f, 0.0f};
    if (!sp) sp = none;
    const char* lim_state = "none";
    float lim_peak = -1.0f;
    if (S.limiter) {
        FMOD_BOOL byp = 1;
        FMOD_DSP_GetBypass(S.limiter, &byp);
        lim_state = byp ? "bypassed" : "on";
        FMOD_DSP_METERING_INFO lo;
        memset(&lo, 0, sizeof(lo));
        FMOD_DSP_SetMeteringEnabled(S.limiter, 0, 1);
        if (FMOD_DSP_GetMeteringInfo(S.limiter, NULL, &lo) == FMOD_OK && lo.numchannels > 0) lim_peak = lo.peaklevel[0];
    }
    char name[128] = "?";
    if (S.up && drivers > 0) FMOD_System_GetDriverInfo(S.sys, 0, name, sizeof(name), NULL, NULL, NULL, NULL);
    _snprintf(buf, len - 1, "sounds %u loaded / %u failed, plays %u / %u failed, %d voices playing, master volume %.2f, output %s (%d devices), scene %s, pathing %s (%d probes, bake %ld/1000, stage %ld, fault %ld code %08lx), limiter %s peak %.3f, enhanced %d, features %d, cpu direct %.2f ms (max %.2f) reflections %.1f ms (max %.1f) quality %d/%d rays %d, nan blocks %ld, direct runs %u, mean occlusion %.2f over %d, master peak %.3f, listener (%.1f %.1f %.1f) a source (%.1f %.1f %.1f) mesh (%.0f %.0f %.0f)-(%.0f %.0f %.0f), last error: %s",
              S.loads, S.load_fails, S.plays, S.play_fails, playing, vol, name, drivers, S.have_scene ? "yes" : "no",
              S.path_state == 2 ? "ready" : S.path_state == 1 ? "baking" : S.path_state == 3 ? "failed" : "off", S.num_probes, bake_permille, S.bake_stage, S.bake_fault, (unsigned long)S.bake_code,
              lim_state, lim_peak, S.enhanced, S.features, S.direct_ms, S.direct_max, S.refl_ms, S.refl_max, S.quality, S.refl_quality, S.refl_rays, S.nan_blocks,
              S.direct_runs, nocc ? occ / nocc : 1.0f, nocc, peak, S.listener[0], S.listener[1], S.listener[2], sp[0], sp[1], sp[2],
              S.mesh_lo[0], S.mesh_lo[1], S.mesh_lo[2], S.mesh_hi[0], S.mesh_hi[1], S.mesh_hi[2], g_err[0] ? g_err : "none");
    buf[len - 1] = 0;
}

/* Once per frame: the simulation inputs, the direct simulation, FMOD. */
/* tests: the spatializer's parameters as the loaded plugin defines them */
static void trace_params(FMOD_DSP* dsp) {
    int n = 0;
    if (!S.trace || !dsp || FMOD_DSP_GetNumParameters(dsp, &n) != FMOD_OK) return;
    for (int i = 0; i < n; i++) {
        FMOD_DSP_PARAMETER_DESC* d = NULL;
        if (FMOD_DSP_GetParameterInfo(dsp, i, &d) != FMOD_OK || !d) continue;
        char vs[64] = "";
        if (d->type == FMOD_DSP_PARAMETER_TYPE_FLOAT) {
            float f = 0;
            FMOD_DSP_GetParameterFloat(dsp, i, &f, NULL, 0);
            _snprintf(vs, sizeof(vs) - 1, "float %.3f", f);
        } else if (d->type == FMOD_DSP_PARAMETER_TYPE_INT) {
            int v = 0;
            FMOD_DSP_GetParameterInt(dsp, i, &v, NULL, 0);
            _snprintf(vs, sizeof(vs) - 1, "int %d", v);
        } else if (d->type == FMOD_DSP_PARAMETER_TYPE_BOOL) {
            FMOD_BOOL v = 0;
            FMOD_DSP_GetParameterBool(dsp, i, &v, NULL, 0);
            _snprintf(vs, sizeof(vs) - 1, "bool %d", v);
        } else {
            _snprintf(vs, sizeof(vs) - 1, "data");
        }
        fprintf(S.trace, "param %d %s = %s\n", i, d->name, vs);
    }
    fflush(S.trace);
}

static void trace_line(void) {
    float dsp = 0, stream = 0, geom = 0, upd = 0, total = 0;
    FMOD_CPU_USAGE cu;
    memset(&cu, 0, sizeof(cu));
    FMOD_System_GetCPUUsage(S.sys, &cu);
    dsp = cu.dsp; stream = cu.stream; geom = cu.geometry; upd = cu.update;
    total = dsp + stream + geom + upd;
    int real = 0, all = 0;
    FMOD_System_GetChannelsPlaying(S.sys, &all, &real);
    Voice* v = &S.v[S.last_voice];
    float aud = -1, vol = -1;
    FMOD_BOOL virt = 0, pl = 0;
    if (v->ch) {
        FMOD_Channel_GetAudibility(v->ch, &aud);
        FMOD_Channel_IsVirtual(v->ch, &virt);
        FMOD_Channel_GetVolume(v->ch, &vol);
        FMOD_Channel_IsPlaying(v->ch, &pl);
    }
    float gv = -1;
    FMOD_BOOL gm = 0, gp = 0;
    FMOD_ChannelGroup_GetVolume(S.sfx, &gv);
    FMOD_ChannelGroup_GetMute(S.sfx, &gm);
    FMOD_ChannelGroup_GetPaused(S.sfx, &gp);
    FMOD_CHANNELGROUP* mg = NULL;
    FMOD_DSP* head = NULL;
    float pk = -1;
    if (FMOD_System_GetMasterChannelGroup(S.sys, &mg) == FMOD_OK && FMOD_ChannelGroup_GetDSP(mg, FMOD_CHANNELCONTROL_DSP_HEAD, &head) == FMOD_OK) {
        FMOD_DSP_METERING_INFO out;
        memset(&out, 0, sizeof(out));
        FMOD_DSP_SetMeteringEnabled(head, 0, 1);
        if (FMOD_DSP_GetMeteringInfo(head, NULL, &out) == FMOD_OK && out.numchannels > 0) pk = out.peaklevel[0];
    }
    float spin = -1, spout = -1;
    if (v->dsp && !v->local) {
        FMOD_DSP_METERING_INFO mi, mo;
        memset(&mi, 0, sizeof(mi));
        memset(&mo, 0, sizeof(mo));
        if (FMOD_DSP_GetMeteringInfo(v->dsp, &mi, &mo) == FMOD_OK) {
            spin = mi.numchannels ? mi.peaklevel[0] : -2;
            spout = mo.numchannels ? mo.peaklevel[0] : -2;
        }
    }
    unsigned int slen = 0;
    float sfreq = 0;
    FMOD_SOUND* snd = NULL;
    if (v->ch && FMOD_Channel_GetCurrentSound(v->ch, &snd) == FMOD_OK && snd) {
        FMOD_Sound_GetLength(snd, &slen, FMOD_TIMEUNIT_MS);
        FMOD_Sound_GetDefaults(snd, &sfreq, NULL);
    }
    unsigned int posms = 0;
    if (v->ch) FMOD_Channel_GetPosition(v->ch, &posms, FMOD_TIMEUNIT_MS);
    fprintf(S.trace, "spat in %.4f out %.4f | sound %u ms at %u ms, %.0f Hz | ", spin, spout, slen, posms, sfreq);
    fprintf(S.trace, "u%u t%lu cpu %.1f ch %d/%d real | voice %d playing %d virtual %d vol %.2f audib %.4f local %d | sfx vol %.2f mute %d pause %d | master peak %.4f | plays %u occl %.2f\n",
            S.updates, GetTickCount(), total, all, real, S.last_voice, pl, virt, vol, aud, v->local, gv, gm, gp, pk, S.plays, v->occlusion);
    fflush(S.trace);
}

__declspec(dllexport) void ta_update(void) {
    if (!S.up) return;
    S.updates++;
    for (int i = 0; i < TA_VOICES; i++)
        if (S.v[i].active && !S.v[i].local) unpoison(S.v[i].dsp);
    unpoison(S.reverb_dsp);
    if (S.trace && S.updates == 1) trace_params(S.v[0].dsp);
    if (S.trace && S.updates % 30 == 0) trace_line();
    if (S.tone_frames > 0) {
        if (!S.tone) {
            FMOD_DSP* osc = NULL;
            if (FMOD_System_CreateDSPByType(S.sys, FMOD_DSP_TYPE_OSCILLATOR, &osc) == FMOD_OK) {
                FMOD_DSP_SetParameterInt(osc, FMOD_DSP_OSCILLATOR_TYPE, 0);
                FMOD_DSP_SetParameterFloat(osc, FMOD_DSP_OSCILLATOR_RATE, 440.0f);
                if (FMOD_System_PlayDSP(S.sys, osc, S.sfx, 0, &S.tone) == FMOD_OK) FMOD_Channel_SetVolume(S.tone, 0.5f);
            }
        }
        if (--S.tone_frames == 0 && S.tone) FMOD_Channel_Stop(S.tone);
    }
    for (int i = 0; i < TA_VOICES; i++) {
        Voice* v = &S.v[i];
        if (v->active && !v->local && voice_playing(v)) set_source_position(v);
    }
    if (S.have_scene && TryEnterCriticalSection(&S.lock)) {
        scene_materials(S.enhanced);
        IPLSimulationSharedInputs sh;
        memset(&sh, 0, sizeof(sh));
        sh.listener.origin = iv(&S.listener[0]);
        sh.listener.ahead = iv(&S.listener[3]);
        sh.listener.up = iv(&S.listener[6]);
        sh.listener.right = iv(&S.listener[9]);
        sh.numRays = S.refl_rays;
        sh.numBounces = 8;
        sh.duration = S.refl_quality < 2 ? 1.5f : S.refl_quality == 2 ? 1.0f : 0.7f;
        sh.order = 1;
        sh.irradianceMinDistance = 1.0f;
        /* every simulation the listener takes part in: pathing included
         * (without it the paths were found to the map's origin) */
        iplSimulatorSetSharedInputs(S.sim, (IPLSimulationFlags)(IPL_SIMULATIONFLAGS_DIRECT | IPL_SIMULATIONFLAGS_REFLECTIONS | IPL_SIMULATIONFLAGS_PATHING), &sh);
        for (int i = 0; i < TA_VOICES; i++) {
            Voice* v = &S.v[i];
            if (!v->active || v->local || v->own) continue;
            IPLSimulationInputs in;
            memset(&in, 0, sizeof(in));
            in.flags = IPL_SIMULATIONFLAGS_DIRECT;
            in.directFlags = (IPLDirectSimulationFlags)(IPL_DIRECTSIMULATIONFLAGS_OCCLUSION | IPL_DIRECTSIMULATIONFLAGS_TRANSMISSION);
            in.source.origin = iv(v->pos);
            in.source.ahead.z = -1.0f;
            in.source.up.y = 1.0f;
            in.source.right.x = 1.0f;
            in.distanceAttenuationModel.type = IPL_DISTANCEATTENUATIONTYPE_DEFAULT;
            in.airAbsorptionModel.type = IPL_AIRABSORPTIONTYPE_DEFAULT;
            in.occlusionType = IPL_OCCLUSIONTYPE_VOLUMETRIC;
            in.occlusionRadius = 0.6f;
            in.numOcclusionSamples = S.quality == 0 ? 12 : S.quality == 1 ? 8 : 4;
            in.numTransmissionRays = S.quality == 0 ? 2 : 1;
            iplSourceSetInputs(v->src, IPL_SIMULATIONFLAGS_DIRECT, &in);
        }
        /* enhanced: the extra sources (reflections for loud sounds, paths
         * around corners once the map is baked) join while needed */
        int changed = 0;
        /* pathing needs the occlusion: its level is the blocked share of
         * the direct sound (below), else a visible source would play twice */
        int path_ok = S.path_state == 2 && S.probes && S.pathing_on && S.occlusion;
        /* the budget: own reflections for at most 8 loud sounds, paths for
         * at most 24 sounds (by voice order), fewer at a lower quality */
        static const int refl_cap[4] = {8, 4, 2, 1};
        int refl_left = refl_cap[S.refl_quality];
        if (S.quality > 0 && refl_left > 2) refl_left = 2;
        int path_left = S.quality == 0 ? 24 : S.quality == 1 ? 16 : 8;
        for (int i = 0; i < TA_VOICES; i++) {
            Voice* v = &S.v[i];
            int live = v->active && !v->local && !v->own && voice_playing(v);
            int refl = live && S.enhanced && S.reflections && v->big && refl_left > 0;
            int path = live && S.enhanced && path_ok && path_left > 0;
            refl_left -= refl;
            path_left -= path;
            int want = v->active && !v->local && !v->own && voice_playing(v) && (refl || path);
            if (want != v->x_added) {
                if (want) iplSourceAdd(v->xsrc, S.sim);
                else iplSourceRemove(v->xsrc, S.sim);
                v->x_added = want;
                changed = 1;
                if (!want) {
                    /* out of the simulator, its outputs freeze: the
                     * spatializer must stop applying them (else the last
                     * path / reflections keep playing) */
                    FMOD_DSP_SetParameterBool(v->dsp, SPAT_APPLY_REFLECTIONS, 0);
                    FMOD_DSP_SetParameterBool(v->dsp, SPAT_APPLY_PATHING, 0);
                    v->path_mix = 0.0f;
                }
            }
            if (!want) continue;
            IPLSimulationInputs in;
            memset(&in, 0, sizeof(in));
            in.flags = (IPLSimulationFlags)((refl ? IPL_SIMULATIONFLAGS_REFLECTIONS : 0) | (path ? IPL_SIMULATIONFLAGS_PATHING : 0));
            in.source.origin = iv(v->pos);
            in.source.ahead.z = -1.0f;
            in.source.up.y = 1.0f;
            in.source.right.x = 1.0f;
            in.distanceAttenuationModel.type = IPL_DISTANCEATTENUATIONTYPE_DEFAULT;
            in.airAbsorptionModel.type = IPL_AIRABSORPTIONTYPE_DEFAULT;
            in.reverbScale[0] = in.reverbScale[1] = in.reverbScale[2] = 1.0f;
            in.hybridReverbTransitionTime = 1.0f; in.hybridReverbOverlapPercent = 0.25f; /* validation: must be > 0 even unused */
            if (path) {
                in.distanceAttenuationModel.type = IPL_DISTANCEATTENUATIONTYPE_CALLBACK;
                in.distanceAttenuationModel.callback = game_falloff;
                in.pathingProbes = S.probes;
                in.visRadius = 0.5f;
                in.visThreshold = 0.1f;
                in.visRange = 50.0f;
                in.pathingOrder = 1;
                /* the map never changes shape (movers aside): the baked
                 * paths stay valid, so no per-pass ray validation or
                 * real-time re-search (they were most of the worker's time) */
                in.enableValidation = IPL_FALSE;
                in.findAlternatePaths = IPL_FALSE;
            }
            iplSourceSetInputs(v->xsrc, in.flags, &in);
            FMOD_DSP_SetParameterBool(v->dsp, SPAT_APPLY_REFLECTIONS, refl);
            FMOD_DSP_SetParameterBool(v->dsp, SPAT_APPLY_PATHING, path);
        }
        if (changed) iplSimulatorCommit(S.sim);
        if (S.reverb_src) {
            IPLSimulationInputs in;
            memset(&in, 0, sizeof(in));
            in.flags = IPL_SIMULATIONFLAGS_REFLECTIONS;
            in.source = sh.listener;
            in.reverbScale[0] = in.reverbScale[1] = in.reverbScale[2] = 1.0f;
            in.hybridReverbTransitionTime = 1.0f; in.hybridReverbOverlapPercent = 0.25f; /* validation: must be > 0 even unused */
            in.distanceAttenuationModel.type = IPL_DISTANCEATTENUATIONTYPE_DEFAULT;
            in.airAbsorptionModel.type = IPL_AIRABSORPTIONTYPE_DEFAULT;
            iplSourceSetInputs(S.reverb_src, IPL_SIMULATIONFLAGS_REFLECTIONS, &in);
        }
        if (S.occlusion) {
            double t0 = now_s();
            iplSimulatorRunDirect(S.sim);
            double ms = (now_s() - t0) * 1000.0;
            S.direct_runs++;
            S.direct_acc += ms;
            if (ms > S.direct_max) S.direct_max = ms;
            if (++S.direct_cnt >= 60) {
                S.direct_ms = S.direct_acc / S.direct_cnt;
                S.direct_acc = 0.0;
                S.direct_cnt = 0;
                /* the budget: ~1.5 ms of the game thread per frame */
                if (S.direct_ms > 1.5 && S.quality < 2) S.quality++;
                else if (S.direct_ms < 0.5 && S.quality > 0) S.quality--;
            }
            for (int i = 0; i < TA_VOICES; i++) {
                Voice* v = &S.v[i];
                if (!v->active || v->local || v->own) continue;
                IPLSimulationOutputs out;
                memset(&out, 0, sizeof(out));
                iplSourceGetOutputs(v->src, IPL_SIMULATIONFLAGS_DIRECT, &out);
                voice_direct(v, out.direct.occlusion, out.direct.transmission);
                /* the path around the walls fills in what the walls block:
                 * none for a visible source (the direct sound is that path),
                 * all of it for a hidden one */
                if (v->x_added) {
                    v->path_mix = 1.0f - out.direct.occlusion;
                    FMOD_DSP_SetParameterFloat(v->dsp, SPAT_PATHING_MIXLEVEL, v->path_mix);
                }
            }
        }
        LeaveCriticalSection(&S.lock);
    }
    for (int i = 0; i < TA_VOICES; i++) {
        if (S.v[i].active && !voice_playing(&S.v[i])) S.v[i].active = 0;
    }
    doppler();
    if (S.meter_voice >= 0 && S.meter_voice < TA_VOICES && S.v[S.meter_voice].active) {
        FMOD_DSP_METERING_INFO mo;
        memset(&mo, 0, sizeof(mo));
        if (FMOD_DSP_GetMeteringInfo(S.v[S.meter_voice].dsp, NULL, &mo) == FMOD_OK && mo.numchannels > 0) {
            float r = 0.0f;
            for (int c = 0; c < mo.numchannels && c < 32; c++) r += mo.rmslevel[c] * mo.rmslevel[c];
            S.meter_sum += sqrt(r / mo.numchannels);
            S.meter_n++;
        }
    }
    FMOD_System_Update(S.sys);
}

/* The enhanced sound features, already resolved against the mode by the
 * caller (vanilla passes hrtf 1, everything else 0): bit 0 binaural
 * (headphones; else panning for speakers), 1 air absorption, 2 doppler,
 * 3 wall thickness, 4 pathing (the baked data stays loaded), 5 the
 * enhanced mode itself (frequency-dependent transmission). */
__declspec(dllexport) void ta_features(int bits) {
    if (!S.up || bits == S.features) return;
    S.features = bits;
    S.hrtf_on = (bits & 1) != 0;
    S.air_on = (bits & 2) != 0;
    S.doppler_on = (bits & 4) != 0;
    S.thick_on = (bits & 8) != 0;
    S.pathing_on = (bits & 16) != 0;
    for (int i = 0; i < TA_VOICES; i++) {
        FMOD_DSP* d = S.v[i].dsp;
        if (!d) continue;
        FMOD_DSP_SetParameterBool(d, SPAT_DIRECT_BINAURAL, S.hrtf_on);
        FMOD_DSP_SetParameterBool(d, SPAT_PATHING_BINAURAL, S.hrtf_on);
        FMOD_DSP_SetParameterBool(d, SPAT_REFLECTIONS_BINAURAL, S.hrtf_on);
        FMOD_DSP_SetParameterInt(d, SPAT_APPLY_AIRABSORPTION, S.air_on ? 1 : 0);
        /* frequency-dependent transmission (enhanced: walls pass the lows,
         * not the highs; vanilla keeps the band average) */
        FMOD_DSP_SetParameterInt(d, SPAT_TRANSMISSION_TYPE, (bits & 32) ? 1 : 0);
    }
    if (S.reverb_dsp) FMOD_DSP_SetParameterBool(S.reverb_dsp, REVERB_BINAURAL, S.hrtf_on);
}

/* Metres of solid between a voice and the listener (the caller traces the
 * map's brushes); used when wall thickness is on. */
__declspec(dllexport) void ta_voice_thickness(int h, float metres) {
    Voice* v = voice_of(h);
    if (S.up && v) v->thick = metres > 0.0f ? metres : 0.0f;
}

/* Tests: a voice's state: doppler factor, occlusion, pathing mix,
 * thickness, transmission (mid), extra source in, then the CPU figures
 * (direct ms, its max, reflection pass ms, its max, quality, rays). */
__declspec(dllexport) int ta_voice_info(int h, float* out, int n) {
    Voice* v = voice_of(h);
    float a[24] = {0};
    if (v) {
        a[0] = v->doppler;
        a[1] = v->occlusion;
        a[2] = v->path_mix;
        a[3] = v->thick;
        a[4] = v->trans_mid;
        a[5] = (float)v->x_added;
        /* what the spatializer holds: occlusion, transmission low / mid /
         * high, transmission type, air absorption apply, pathing apply,
         * and its input / output RMS (first channel) */
        if (v->dsp) {
            FMOD_DSP_GetParameterFloat(v->dsp, SPAT_OCCLUSION, &a[12], NULL, 0);
            FMOD_DSP_GetParameterFloat(v->dsp, SPAT_TRANSMISSION_LOW, &a[13], NULL, 0);
            FMOD_DSP_GetParameterFloat(v->dsp, SPAT_TRANSMISSION_MID, &a[14], NULL, 0);
            FMOD_DSP_GetParameterFloat(v->dsp, SPAT_TRANSMISSION_HIGH, &a[15], NULL, 0);
            int tt = 0, aa = 0;
            FMOD_BOOL ap = 0;
            FMOD_DSP_GetParameterInt(v->dsp, SPAT_TRANSMISSION_TYPE, &tt, NULL, 0);
            FMOD_DSP_GetParameterInt(v->dsp, SPAT_APPLY_AIRABSORPTION, &aa, NULL, 0);
            FMOD_DSP_GetParameterBool(v->dsp, SPAT_APPLY_PATHING, &ap, NULL, 0);
            a[16] = (float)(tt * 100 + aa * 10 + ap);
            FMOD_DSP_METERING_INFO mi, mo;
            memset(&mi, 0, sizeof(mi));
            memset(&mo, 0, sizeof(mo));
            if (FMOD_DSP_GetMeteringInfo(v->dsp, &mi, &mo) == FMOD_OK) {
                a[17] = mi.numchannels ? mi.rmslevel[0] : -1.0f;
                a[18] = mo.numchannels ? mo.rmslevel[0] : -1.0f;
            }
            FMOD_BOOL act = 0, byp = 0;
            FMOD_DSP_GetActive(v->dsp, &act);
            FMOD_DSP_GetBypass(v->dsp, &byp);
            a[19] = (float)(act * 10 + byp);
        }
        /* the extra source's outputs: pathing EQ (mean), its first SH
         * coefficient, and whether a reflection IR is there */
        if (v->x_added) {
            IPLSimulationOutputs o;
            memset(&o, 0, sizeof(o));
            iplSourceGetOutputs(v->xsrc, (IPLSimulationFlags)(IPL_SIMULATIONFLAGS_REFLECTIONS | IPL_SIMULATIONFLAGS_PATHING), &o);
            a[20] = (o.pathing.eqCoeffs[0] + o.pathing.eqCoeffs[1] + o.pathing.eqCoeffs[2]) / 3.0f;
            a[21] = o.pathing.shCoeffs ? o.pathing.shCoeffs[0] : -1.0f;
            a[22] = o.reflections.ir ? 1.0f : 0.0f;
            FMOD_BOOL ar = 0;
            if (v->dsp) FMOD_DSP_GetParameterBool(v->dsp, SPAT_APPLY_REFLECTIONS, &ar, NULL, 0);
            a[23] = (float)ar;
        }
    }
    a[6] = (float)S.direct_ms;
    a[7] = (float)S.direct_max;
    a[8] = (float)S.refl_ms;
    a[9] = (float)S.refl_max;
    a[10] = (float)(S.quality + 10 * S.refl_quality); /* direct level + 10 x reflection level */
    a[11] = (float)S.refl_rays;
    for (int i = 0; i < n && i < 24; i++) out[i] = a[i];
    return v != NULL;
}

/* Tests: average a voice's spatialized output RMS from now on. */
__declspec(dllexport) void ta_voice_meter(int h) {
    S.meter_voice = (h >= 0 && voice_of(h)) ? (h & 0xff) : -1;
    S.meter_sum = 0.0;
    S.meter_n = 0;
}
/* The mean RMS since the last call (and restart the average); -1 none. */
__declspec(dllexport) float ta_voice_meter_read(void) {
    float r = S.meter_n ? (float)(S.meter_sum / S.meter_n) : -1.0f;
    S.meter_sum = 0.0;
    S.meter_n = 0;
    return r;
}

/* Tests (s_pathscan): how pathing treats the floor probes within `radius`
 * of the listener. For each, the direct occlusion and the pathing result
 * (EQ, and the direction its first-order SH points to, against the straight
 * line). A blocked probe should get a path from another direction, a visible
 * one the straight line. `best` gets the blocked probe with the strongest
 * path (for an audible A/B). */
__declspec(dllexport) int ta_path_scan(float radius, char* buf, int len, float* best) {
    buf[0] = 0;
    if (!S.up || !S.have_scene || S.path_state != 2) {
        _snprintf(buf, len - 1, "pathing not ready (state %ld)", S.path_state);
        return 0;
    }
    EnterCriticalSection(&S.lock);
    IPLProbeArray arr = NULL;
    iplProbeArrayCreate(S.ctx, &arr);
    IPLProbeGenerationParams gp;
    memset(&gp, 0, sizeof(gp));
    gp.type = IPL_PROBEGENERATIONTYPE_UNIFORMFLOOR;
    gp.spacing = 3.0f;
    gp.height = 1.5f;
    for (int k = 0; k < 3; k++) {
        gp.transform.elements[k][k] = S.mesh_hi[k] - S.mesh_lo[k];
        gp.transform.elements[k][3] = 0.5f * (S.mesh_lo[k] + S.mesh_hi[k]);
    }
    gp.transform.elements[3][3] = 1.0f;
    iplProbeArrayGenerateProbes(arr, S.scene, &gp);
    int np = iplProbeArrayGetNumProbes(arr);
    IPLSource src = NULL;
    IPLSourceSettings ss;
    ss.flags = (IPLSimulationFlags)(IPL_SIMULATIONFLAGS_DIRECT | IPL_SIMULATIONFLAGS_PATHING);
    iplSourceCreate(S.sim, &ss, &src);
    iplSourceAdd(src, S.sim);
    iplSimulatorCommit(S.sim);
    IPLSimulationSharedInputs sh;
    memset(&sh, 0, sizeof(sh));
    sh.listener.origin = iv(&S.listener[0]);
    sh.listener.ahead = iv(&S.listener[3]);
    sh.listener.up = iv(&S.listener[6]);
    sh.listener.right = iv(&S.listener[9]);
    sh.numRays = 1024;
    sh.numBounces = 8;
    sh.duration = 1.5f;
    sh.order = 1;
    sh.irradianceMinDistance = 1.0f;
    iplSimulatorSetSharedInputs(S.sim, (IPLSimulationFlags)(IPL_SIMULATIONFLAGS_DIRECT | IPL_SIMULATIONFLAGS_PATHING), &sh);
    int n = 0, vis = 0, vis_path = 0, blocked = 0, blocked_path = 0;
    double vis_ang = 0, blk_ang = 0, blk_eq = 0, best_eq = 0;
    for (int i = 0; i < np && n < 200; i++) {
        IPLSphere p = iplProbeArrayGetProbe(arr, i);
        float pc[3] = {p.center.x, p.center.y, p.center.z};
        float d[3] = {pc[0] - S.listener[0], pc[1] - S.listener[1], pc[2] - S.listener[2]};
        float dist = sqrtf(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]);
        if (dist > radius || dist < 2.0f) continue;
        n++;
        IPLSimulationInputs in;
        memset(&in, 0, sizeof(in));
        in.flags = (IPLSimulationFlags)(IPL_SIMULATIONFLAGS_DIRECT | IPL_SIMULATIONFLAGS_PATHING);
        in.directFlags = IPL_DIRECTSIMULATIONFLAGS_OCCLUSION;
        in.source.origin = p.center;
        in.source.ahead.z = -1.0f;
        in.source.up.y = 1.0f;
        in.source.right.x = 1.0f;
        in.distanceAttenuationModel.type = IPL_DISTANCEATTENUATIONTYPE_DEFAULT;
        in.airAbsorptionModel.type = IPL_AIRABSORPTIONTYPE_DEFAULT;
        in.occlusionType = IPL_OCCLUSIONTYPE_VOLUMETRIC;
        in.occlusionRadius = 0.6f;
        in.numOcclusionSamples = 12;
        in.reverbScale[0] = in.reverbScale[1] = in.reverbScale[2] = 1.0f;
        in.hybridReverbTransitionTime = 1.0f;
        in.hybridReverbOverlapPercent = 0.25f;
        in.pathingProbes = S.probes;
        in.distanceAttenuationModel.type = IPL_DISTANCEATTENUATIONTYPE_CALLBACK;
        in.distanceAttenuationModel.callback = game_falloff;
        in.visRadius = 0.5f;
        in.visThreshold = 0.1f;
        in.visRange = 50.0f;
        in.pathingOrder = 1;
        in.enableValidation = IPL_FALSE; /* as the voices (ta_update) */
        in.findAlternatePaths = IPL_FALSE;
        iplSourceSetInputs(src, in.flags, &in);
        iplSimulatorRunDirect(S.sim);
        iplSimulatorRunPathing(S.sim);
        IPLSimulationOutputs out;
        memset(&out, 0, sizeof(out));
        iplSourceGetOutputs(src, (IPLSimulationFlags)(IPL_SIMULATIONFLAGS_DIRECT | IPL_SIMULATIONFLAGS_PATHING), &out);
        float eq = (out.pathing.eqCoeffs[0] + out.pathing.eqCoeffs[1] + out.pathing.eqCoeffs[2]) / 3.0f;
        /* SH order 1 (Google SH, +x forward +y left +z up, from Steam
         * Audio's +x right +y up +z back): idx1 ~ -x, idx2 ~ y, idx3 ~ -z */
        float a = 0.0f;
        int has = 0;
        if (out.pathing.shCoeffs) {
            float* c = out.pathing.shCoeffs;
            float pd[3] = {-c[1], c[2], -c[3]};
            float pl = sqrtf(pd[0] * pd[0] + pd[1] * pd[1] + pd[2] * pd[2]);
            has = pl > 1e-6f && eq > 1e-4f;
            if (has) a = acosf(fmaxf(-1.0f, fminf(1.0f, (pd[0] * d[0] + pd[1] * d[1] + pd[2] * d[2]) / (pl * dist)))) * 57.29578f;
        }
        if (out.direct.occlusion > 0.99f) {
            static float far_d;
            if (vis == 0) far_d = 0.0f;
            if (dist > far_d) {
                far_d = dist;
                memcpy(S.far_visible, pc, sizeof(pc));
            }
        }
        if (out.direct.occlusion > 0.9f) {
            vis++;
            if (has) { vis_path++; vis_ang += a; }
        } else if (out.direct.occlusion < 0.1f) {
            blocked++;
            if (has) {
                blocked_path++;
                blk_ang += a;
                blk_eq += eq;
                if (eq > best_eq) {
                    best_eq = eq;
                    memcpy(S.best_path, pc, sizeof(pc));
                }
            }
        }
    }
    iplSourceRemove(src, S.sim);
    iplSimulatorCommit(S.sim);
    iplSourceRelease(&src);
    iplProbeArrayRelease(&arr);
    LeaveCriticalSection(&S.lock);
    if (best) {
        memcpy(best, S.best_path, sizeof(S.best_path));
        memcpy(best + 3, S.far_visible, sizeof(S.far_visible));
    }
    _snprintf(buf, len - 1, "path scan: %d probes within %.0f of the listener (%.1f %.1f %.1f); visible %d (path %d, mean angle %.1f deg); blocked %d (path %d, mean angle %.1f deg, mean eq %.3f); best (%.1f %.1f %.1f) eq %.3f",
              n, radius, S.listener[0], S.listener[1], S.listener[2], vis, vis_path, vis_path ? vis_ang / vis_path : 0.0, blocked, blocked_path,
              blocked_path ? blk_ang / blocked_path : 0.0, blocked_path ? blk_eq / blocked_path : 0.0, S.best_path[0], S.best_path[1], S.best_path[2], best_eq);
    buf[len - 1] = 0;
    return blocked_path;
}
