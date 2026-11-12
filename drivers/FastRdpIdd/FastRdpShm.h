// FastRdpShm.h - per-session frame shared memory between FastRdpIdd (producer, WUDFHost / LOCAL SERVICE)
// and fastrdp-server (consumer, LOCAL SERVICE). Rust mirror: crates/frame-shm/src/lib.rs.
//
//   Section : Global\FastRDP_Frame_S<sessionId>     (pagefile-backed, DACL: SYSTEM, LOCAL SERVICE, Administrators)
//   Event   : Global\FastRDP_FrameEvt_S<sessionId>  (auto-reset, signalled after each published frame
//             and after each hardware-cursor update)
//   Event   : Global\FastRDP_CtlEvt_S<sessionId>    (auto-reset, signalled by the server after a display request)
//   Either side may create the section first (same name, size and DACL): the server writes the requested
//   display mode before the driver starts, so the session comes up at the client's resolution.
//
// Layout: [Header 256 B][Slot meta x3 (64 B each)] at 0, cursor state (64 B) at CURSOR_OFFSET,
//         display control (64 B) at CONTROL_OFFSET,
//         cursor shape (<= 256x256x4) at CURSOR_SHAPE_OFFSET, frame data at DATA_OFFSET:
//         slot i pixels at DATA_OFFSET + i * SlotBytes, BGRA, stride = width * 4.
// Cursor: hardware cursor reported by IddCx (the OS does NOT draw it into the frames). Seqlock like
//         the slots; the shape bytes are only rewritten when shape_id changes. Shape types follow
//         IDDCX_CURSOR_SHAPE_TYPE (1 = masked color: BGRA, A=0x00 opaque, A=0xFF XOR with screen;
//         2 = alpha: BGRA straight alpha).
// Publication: producer picks slot (latest+1)%3, seq++ (odd = writing), writes meta+pixels, seq++ (even),
//              then latest_frame / latest_slot, then SetEvent. Consumer copies the latest slot and accepts it
//              only if seq was even and unchanged across the copy (seqlock).
#pragma once
#include <stdint.h>

#define FASTRDP_SHM_MAGIC        0x4D485346u  /* 'FSHM' */
#define FASTRDP_SHM_VERSION      3u
#define FASTRDP_SHM_SLOTS        3u
#define FASTRDP_SHM_MAX_WIDTH    3840u
#define FASTRDP_SHM_MAX_HEIGHT   2160u
#define FASTRDP_SHM_SLOT_BYTES   ((uint64_t)FASTRDP_SHM_MAX_WIDTH * FASTRDP_SHM_MAX_HEIGHT * 4u)
#define FASTRDP_SHM_CURSOR_OFFSET        1024u
#define FASTRDP_SHM_CONTROL_OFFSET       2048u
#define FASTRDP_SHM_CURSOR_SHAPE_OFFSET  4096u
#define FASTRDP_SHM_CURSOR_MAX           256u
#define FASTRDP_SHM_CURSOR_SHAPE_BYTES   (FASTRDP_SHM_CURSOR_MAX * FASTRDP_SHM_CURSOR_MAX * 4u)
#define FASTRDP_SHM_DATA_OFFSET  (FASTRDP_SHM_CURSOR_SHAPE_OFFSET + FASTRDP_SHM_CURSOR_SHAPE_BYTES)
#define FASTRDP_SHM_TOTAL_BYTES  (FASTRDP_SHM_DATA_OFFSET + FASTRDP_SHM_SLOTS * FASTRDP_SHM_SLOT_BYTES)
#define FASTRDP_SHM_FORMAT_BGRA8 87u          /* DXGI_FORMAT_B8G8R8A8_UNORM */

#pragma pack(push, 8)
typedef struct FastRdpShmSlot {
    volatile int64_t seq;          /* even = stable, odd = being written */
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    uint32_t format;
    int64_t  pts_us;               /* QueryPerformanceCounter time of acquisition, microseconds */
    uint64_t frame_number;         /* driver frame counter (monotonic per swap-chain processor) */
    uint32_t dirty_rects;
    uint32_t reserved[5];
} FastRdpShmSlot;                  /* 64 bytes */

typedef struct FastRdpShmHeader {
    uint32_t magic;
    uint32_t version;
    uint32_t header_size;          /* sizeof(FastRdpShmHeader) */
    uint32_t slot_count;
    uint64_t slot_bytes;
    uint64_t data_offset;
    uint32_t session_id;
    uint32_t producer_pid;
    volatile int64_t latest_frame; /* frame_number of the latest complete slot, -1 = none yet */
    volatile int32_t latest_slot;
    uint32_t reserved0;
    volatile int64_t producer_heartbeat_us; /* updated at least every ~1 s while the swap-chain thread runs */
    uint8_t  reserved[256 - 64];
    FastRdpShmSlot slots[FASTRDP_SHM_SLOTS];
} FastRdpShmHeader;

typedef struct FastRdpShmCursor {
    volatile int64_t seq;          /* even = stable, odd = being written */
    uint32_t visible;
    int32_t  x;                    /* top-left of the cursor image in desktop pixels (may be negative) */
    int32_t  y;
    uint32_t shape_id;             /* changes whenever the shape bytes change; 0 = no shape yet */
    uint32_t shape_type;           /* IDDCX_CURSOR_SHAPE_TYPE */
    uint32_t width;
    uint32_t height;
    uint32_t pitch;
    uint32_t x_hot;
    uint32_t y_hot;
    uint32_t updates;              /* producer counter (diagnostics) */
    uint32_t reserved[3];
} FastRdpShmCursor;                /* 64 bytes */

/* Display mode requested by the client (via the server) and the driver's answer. Request fields are
   written by the server under `seq` (seqlock); applied_* only by the driver. */
typedef struct FastRdpShmControl {
    volatile int64_t seq;
    uint32_t request_id;           /* increments per request, 0 = none */
    uint32_t width;                /* desktop pixels */
    uint32_t height;
    uint32_t refresh_hz;
    uint32_t scale_percent;        /* 100..500 (monitor DPI hint) */
    volatile uint32_t applied_id;  /* request_id the driver last processed */
    volatile int32_t applied_status; /* NTSTATUS of IddCxAdapterDisplayConfigUpdate */
    uint32_t applied_width;
    uint32_t applied_height;
    uint32_t applied_scale;
    uint32_t reserved[4];
} FastRdpShmControl;               /* 64 bytes */
#pragma pack(pop)

#ifdef __cplusplus
static_assert(sizeof(FastRdpShmSlot) == 64, "slot meta size");
static_assert(sizeof(FastRdpShmHeader) == 256 + 3 * 64, "header size");
static_assert(sizeof(FastRdpShmCursor) == 64, "cursor size");
static_assert(sizeof(FastRdpShmControl) == 64, "control size");
static_assert(sizeof(FastRdpShmHeader) <= FASTRDP_SHM_CURSOR_OFFSET, "cursor after header");
#endif
