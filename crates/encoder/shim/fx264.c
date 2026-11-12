/* fx264.c - minimal C shim over libx264 (GPLv2) so the Rust side never depends on x264_param_t layout.
   Built by crates/encoder/build.rs with MSVC, linked against a static libx264 (vcpkg x64-windows-static). */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <x264.h>

typedef struct fx264 {
    x264_t* h;
    int width, height;
} fx264;

typedef struct fx264_params {
    int width, height, fps;
    int bitrate_kbps, vbv_ms;
    int threads;            /* 0 = auto */
    int keyint;             /* 0 = infinite (IDR only on request) */
    int intra_refresh;      /* periodic intra refresh instead of IDR frames */
    const char* preset;     /* e.g. "superfast" */
    const char* tune;       /* e.g. "zerolatency" (may be NULL) */
    const char* profile;    /* e.g. "high" */
} fx264_params;

const char* fx264_version(void) { return X264_POINTVER; }
int fx264_build(void) { return X264_BUILD; }

int fx264_open(fx264** out, const fx264_params* prm, char* err, int errlen)
{
    x264_param_t p;
    *out = NULL;
    if (x264_param_default_preset(&p, prm->preset, prm->tune) < 0) {
        snprintf(err, errlen, "invalid preset/tune '%s'/'%s'", prm->preset, prm->tune ? prm->tune : "");
        return -1;
    }
    p.i_width = prm->width;
    p.i_height = prm->height;
    p.i_csp = X264_CSP_I420;
    p.i_fps_num = (uint32_t)prm->fps;
    p.i_fps_den = 1;
    p.i_threads = prm->threads;
    p.i_log_level = X264_LOG_WARNING;
    p.b_repeat_headers = 1;   /* SPS/PPS before every IDR: clients can join at any keyframe */
    p.b_annexb = 1;
    p.rc.i_rc_method = X264_RC_ABR;
    p.rc.i_bitrate = prm->bitrate_kbps;
    p.rc.i_vbv_max_bitrate = prm->bitrate_kbps;
    p.rc.i_vbv_buffer_size = prm->bitrate_kbps * (prm->vbv_ms > 0 ? prm->vbv_ms : 200) / 1000;
    if (prm->keyint > 0) p.i_keyint_max = prm->keyint;
    else p.i_keyint_max = X264_KEYINT_MAX_INFINITE;
    p.b_intra_refresh = prm->intra_refresh ? 1 : 0;
    /* BT.709 limited range, matching the BGRA->I420 converter */
    p.vui.i_colorprim = 1;
    p.vui.i_transfer = 1;
    p.vui.i_colmatrix = 1;
    p.vui.b_fullrange = 0;
    if (prm->profile && prm->profile[0] && x264_param_apply_profile(&p, prm->profile) < 0) {
        snprintf(err, errlen, "invalid profile '%s'", prm->profile);
        return -2;
    }
    fx264* e = (fx264*)calloc(1, sizeof(fx264));
    if (!e) { snprintf(err, errlen, "out of memory"); return -3; }
    e->h = x264_encoder_open(&p);
    if (!e->h) { free(e); snprintf(err, errlen, "x264_encoder_open failed"); return -4; }
    e->width = prm->width;
    e->height = prm->height;
    *out = e;
    return 0;
}

/* Encodes one I420 frame. On success *out/*outlen point into x264-owned memory valid until the
   next call. Returns 0 = ok (outlen may be 0 when x264 buffers), <0 = error. */
int fx264_encode(fx264* e, const uint8_t* y, int ys, const uint8_t* u, int us, const uint8_t* v, int vs,
                 int64_t pts, int force_idr, const uint8_t** out, int* outlen, int* keyframe)
{
    x264_picture_t in, outpic;
    x264_nal_t* nals = NULL;
    int nnal = 0;
    x264_picture_init(&in);
    in.img.i_csp = X264_CSP_I420;
    in.img.i_plane = 3;
    in.img.plane[0] = (uint8_t*)y; in.img.i_stride[0] = ys;
    in.img.plane[1] = (uint8_t*)u; in.img.i_stride[1] = us;
    in.img.plane[2] = (uint8_t*)v; in.img.i_stride[2] = vs;
    in.i_pts = pts;
    in.i_type = force_idr ? X264_TYPE_IDR : X264_TYPE_AUTO;
    int size = x264_encoder_encode(e->h, &nals, &nnal, &in, &outpic);
    *out = NULL; *outlen = 0; *keyframe = 0;
    if (size < 0) return -1;
    if (size > 0) {
        *out = nals[0].p_payload;   /* payloads of one call are contiguous */
        *outlen = size;
        *keyframe = outpic.b_keyframe;
    }
    return 0;
}

/* Changes the target bitrate of a running encoder (ABR + VBV), effective from the next frame. */
int fx264_set_bitrate(fx264* e, int bitrate_kbps, int vbv_ms)
{
    x264_param_t p;
    x264_encoder_parameters(e->h, &p);
    p.rc.i_bitrate = bitrate_kbps;
    p.rc.i_vbv_max_bitrate = bitrate_kbps;
    p.rc.i_vbv_buffer_size = bitrate_kbps * (vbv_ms > 0 ? vbv_ms : 200) / 1000;
    return x264_encoder_reconfig(e->h, &p);
}

void fx264_close(fx264* e)
{
    if (!e) return;
    if (e->h) x264_encoder_close(e->h);
    free(e);
}
