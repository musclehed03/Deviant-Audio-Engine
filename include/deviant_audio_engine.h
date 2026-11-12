/* SPDX-License-Identifier: GPL-3.0-only */
#ifndef DEVIANT_AUDIO_ENGINE_H
#define DEVIANT_AUDIO_ENGINE_H
#include <stdint.h>
#include <stddef.h>
#ifdef _WIN32
# define DAE_API __declspec(dllimport)
#else
# define DAE_API
#endif
#ifdef __cplusplus
extern "C" {
#endif
/* ABI v1: opaque integer handles. 0 = invalid; never reuse a shutdown handle.
 * Status: 0 = success, -1 = failure; retrieve thread-local dae_last_error.
 * All text is UTF-8. Caller owns every buffer. No arguments may overlap.
 * JSON/error functions return required capacity INCLUDING NUL (0 = error).
 * NULL/0 queries size; undersized buffers are untouched. Retry if size grows.
 * Functions serialize access. Avoid calling from an audio realtime callback.
 */
DAE_API uint64_t dae_init(void);
DAE_API int32_t dae_shutdown(uint64_t handle);
DAE_API size_t dae_last_error(char *out, size_t capacity);
DAE_API size_t dae_devices_json(char *out, size_t capacity);
/* NULL or empty device_id selects the default render endpoint. Requested rate
 * and channels are fixed; the PCM representation may be negotiated. Always
 * query capabilities after opening. Failure NEVER opens a shared stream. */
DAE_API int32_t dae_open_exclusive(uint64_t handle, const char *device_id,
    uint32_t rate, uint16_t channels, uint16_t container_bits,
    uint16_t valid_bits, uint8_t float_pcm);
/* Stops and discards queued data. Shutdown also closes the decoder. */
DAE_API int32_t dae_close_stream(uint64_t handle);
/* Interleaved float PCM; count is SAMPLES (frames * channels). All-or-nothing
 * enqueue. On queue-full retry the same buffer after output consumes data.
 * The queue holds two seconds. Rate/channels must match negotiated output. */
DAE_API int32_t dae_feed_frames(uint64_t handle, const float *samples,
    size_t sample_count, uint32_t rate, uint16_t channels);
DAE_API int32_t dae_decode_open(uint64_t handle, const char *local_path);
/* capacity counts samples, written counts frames. 0 frames + success = EOF.
 * At most 65536 frames per call; partial channel frames are not written. */
DAE_API int32_t dae_decode_read(uint64_t handle, float *out,
    size_t capacity_samples, size_t *written_frames);
DAE_API int32_t dae_decode_close(uint64_t handle);
/* Offline only: do not reset this interpolator independently on streaming
 * chunks. Supply the complete clip; maximum 16M input/output samples.
 * No antialias filter; downsampling can alias. Prefer opening at source rate. */
DAE_API int32_t dae_resample(const float *input, size_t sample_count,
    uint32_t source_rate, uint32_t target_rate, uint16_t channels,
    float *out, size_t capacity_samples, size_t *written_samples);
DAE_API size_t dae_capabilities_json(uint64_t handle, char *out, size_t capacity);
#ifdef __cplusplus
}
#endif
#endif
