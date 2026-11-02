/*
 * Speex noise suppression as a mono LADSPA plugin, for the Studio Mic denoiser slot.
 * No distro packages a LADSPA wrapper for speexdsp, so Studio Mic builds and bundles this one.
 *
 *   label  : speex_preprocess
 *   ports  : "Input", "Output" (audio), "Noise Suppression (dB)" (control, -60..-1),
 *            "latency" (output control, samples; PipeWire uses it for latency reporting)
 *
 * speex_preprocess works on fixed 10 ms int16 frames, so audio is buffered one frame
 * (10 ms of latency). Samples are scaled by 0.5 into int16 so hot input keeps 6 dB of
 * headroom instead of clipping inside the denoiser.
 *
 * Build: cc -O2 -fPIC -shared -o libspeex_ladspa.so speex_ladspa.c $(pkg-config --cflags --libs speexdsp)
 *
 * The LADSPA 1.1 ABI is declared below so the build doesn't need ladspa.h installed.
 */
#include <speex/speex_preprocess.h>
#include <stdlib.h>
#include <string.h>

typedef float LADSPA_Data;
typedef int LADSPA_Properties;
typedef int LADSPA_PortDescriptor;
typedef int LADSPA_PortRangeHintDescriptor;
typedef void *LADSPA_Handle;
typedef struct {
    LADSPA_PortRangeHintDescriptor HintDescriptor;
    LADSPA_Data LowerBound;
    LADSPA_Data UpperBound;
} LADSPA_PortRangeHint;
typedef struct _LADSPA_Descriptor {
    unsigned long UniqueID;
    const char *Label;
    LADSPA_Properties Properties;
    const char *Name;
    const char *Maker;
    const char *Copyright;
    unsigned long PortCount;
    const LADSPA_PortDescriptor *PortDescriptors;
    const char *const *PortNames;
    const LADSPA_PortRangeHint *PortRangeHints;
    void *ImplementationData;
    LADSPA_Handle (*instantiate)(const struct _LADSPA_Descriptor *, unsigned long);
    void (*connect_port)(LADSPA_Handle, unsigned long, LADSPA_Data *);
    void (*activate)(LADSPA_Handle);
    void (*run)(LADSPA_Handle, unsigned long);
    void (*run_adding)(LADSPA_Handle, unsigned long);
    void (*set_run_adding_gain)(LADSPA_Handle, LADSPA_Data);
    void (*deactivate)(LADSPA_Handle);
    void (*cleanup)(LADSPA_Handle);
} LADSPA_Descriptor;

#define LADSPA_PROPERTY_HARD_RT_CAPABLE 0x4
#define LADSPA_PORT_INPUT 0x1
#define LADSPA_PORT_OUTPUT 0x2
#define LADSPA_PORT_CONTROL 0x4
#define LADSPA_PORT_AUDIO 0x8
#define LADSPA_HINT_BOUNDED_BELOW 0x1
#define LADSPA_HINT_BOUNDED_ABOVE 0x2
#define LADSPA_HINT_DEFAULT_HIGH 0x100
#define LADSPA_HINT_DEFAULT_0 0x200

enum { P_IN, P_OUT, P_SUPPRESS, P_LATENCY, P_COUNT };

#define IN_SCALE 16384.0f /* 0.5 * 32768: 6 dB of headroom before int16 clipping */

typedef struct {
    LADSPA_Data *port[P_COUNT];
    unsigned long rate;
    int frame;
    int pos;
    int suppress;
    SpeexPreprocessState *st;
    spx_int16_t *work;
    float *in_buf;
    float *out_buf;
} Speex;

static void reset_state(Speex *s) {
    if (s->st) speex_preprocess_state_destroy(s->st);
    s->st = speex_preprocess_state_init(s->frame, (int)s->rate);
    int on = 1, off = 0;
    speex_preprocess_ctl(s->st, SPEEX_PREPROCESS_SET_DENOISE, &on);
    speex_preprocess_ctl(s->st, SPEEX_PREPROCESS_SET_AGC, &off);
    speex_preprocess_ctl(s->st, SPEEX_PREPROCESS_SET_VAD, &off);
    speex_preprocess_ctl(s->st, SPEEX_PREPROCESS_SET_DEREVERB, &off);
    speex_preprocess_ctl(s->st, SPEEX_PREPROCESS_SET_NOISE_SUPPRESS, &s->suppress);
    memset(s->in_buf, 0, sizeof(float) * s->frame);
    memset(s->out_buf, 0, sizeof(float) * s->frame);
    s->pos = 0;
}

static LADSPA_Handle instantiate(const LADSPA_Descriptor *d, unsigned long rate) {
    (void)d;
    Speex *s = calloc(1, sizeof(Speex));
    if (!s) return NULL;
    s->rate = rate ? rate : 48000;
    s->frame = (int)(s->rate / 100); /* 10 ms */
    s->suppress = -20;
    s->work = calloc(s->frame, sizeof(spx_int16_t));
    s->in_buf = calloc(s->frame, sizeof(float));
    s->out_buf = calloc(s->frame, sizeof(float));
    if (!s->work || !s->in_buf || !s->out_buf) {
        free(s->work); free(s->in_buf); free(s->out_buf); free(s);
        return NULL;
    }
    reset_state(s);
    return s;
}

static void connect_port(LADSPA_Handle h, unsigned long port, LADSPA_Data *data) {
    if (port < P_COUNT) ((Speex *)h)->port[port] = data;
}

static void activate(LADSPA_Handle h) { reset_state((Speex *)h); }

static void process_frame(Speex *s) {
    for (int i = 0; i < s->frame; i++) {
        float v = s->in_buf[i] * IN_SCALE;
        if (v > 32767.0f) v = 32767.0f;
        if (v < -32768.0f) v = -32768.0f;
        s->work[i] = (spx_int16_t)v;
    }
    speex_preprocess_run(s->st, s->work);
    for (int i = 0; i < s->frame; i++) s->out_buf[i] = (float)s->work[i] / IN_SCALE;
}

static void run(LADSPA_Handle h, unsigned long n) {
    Speex *s = (Speex *)h;
    const LADSPA_Data *in = s->port[P_IN];
    LADSPA_Data *out = s->port[P_OUT];
    if (s->port[P_SUPPRESS]) {
        float want = *s->port[P_SUPPRESS];
        if (want > -1.0f) want = -1.0f;
        if (want < -60.0f) want = -60.0f;
        int db = (int)(want - 0.5f);
        if (db != s->suppress) {
            s->suppress = db;
            speex_preprocess_ctl(s->st, SPEEX_PREPROCESS_SET_NOISE_SUPPRESS, &s->suppress);
        }
    }
    if (s->port[P_LATENCY]) *s->port[P_LATENCY] = (LADSPA_Data)s->frame;
    if (!in || !out) return;
    for (unsigned long i = 0; i < n; i++) {
        float x = in[i]; /* read first: the host may run us in place */
        out[i] = s->out_buf[s->pos];
        s->in_buf[s->pos] = x;
        if (++s->pos == s->frame) {
            process_frame(s);
            s->pos = 0;
        }
    }
}

static void cleanup(LADSPA_Handle h) {
    Speex *s = (Speex *)h;
    if (s->st) speex_preprocess_state_destroy(s->st);
    free(s->work); free(s->in_buf); free(s->out_buf); free(s);
}

static const LADSPA_PortDescriptor port_desc[P_COUNT] = {
    LADSPA_PORT_INPUT | LADSPA_PORT_AUDIO,
    LADSPA_PORT_OUTPUT | LADSPA_PORT_AUDIO,
    LADSPA_PORT_INPUT | LADSPA_PORT_CONTROL,
    LADSPA_PORT_OUTPUT | LADSPA_PORT_CONTROL,
};
static const char *const port_names[P_COUNT] = { "Input", "Output", "Noise Suppression (dB)", "latency" };
static const LADSPA_PortRangeHint port_hints[P_COUNT] = {
    { 0, 0, 0 },
    { 0, 0, 0 },
    { LADSPA_HINT_BOUNDED_BELOW | LADSPA_HINT_BOUNDED_ABOVE | LADSPA_HINT_DEFAULT_HIGH, -60.0f, -1.0f },
    { LADSPA_HINT_BOUNDED_BELOW | LADSPA_HINT_DEFAULT_0, 0.0f, 0.0f },
};

static const LADSPA_Descriptor descriptor = {
    .UniqueID = 9431,
    .Label = "speex_preprocess",
    .Properties = LADSPA_PROPERTY_HARD_RT_CAPABLE,
    .Name = "Studio Mic Speex Noise Suppression (mono)",
    .Maker = "Studio Mic",
    .Copyright = "BSD-3-Clause (speexdsp)",
    .PortCount = P_COUNT,
    .PortDescriptors = port_desc,
    .PortNames = port_names,
    .PortRangeHints = port_hints,
    .ImplementationData = NULL,
    .instantiate = instantiate,
    .connect_port = connect_port,
    .activate = activate,
    .run = run,
    .run_adding = NULL,
    .set_run_adding_gain = NULL,
    .deactivate = NULL,
    .cleanup = cleanup,
};

__attribute__((visibility("default"))) const LADSPA_Descriptor *ladspa_descriptor(unsigned long index) {
    return index == 0 ? &descriptor : NULL;
}
