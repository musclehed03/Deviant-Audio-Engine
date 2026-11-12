"""Exercise the compiled C ABI without an audio device (Python stdlib only)."""
import ctypes as c
import json
import pathlib
import sys

root = pathlib.Path(__file__).resolve().parent.parent
name = "deviant_audio_engine.dll" if sys.platform == "win32" else "libdeviant_audio_engine.so"
lib = c.CDLL(str(root / "target" / "release" / name))
handle = c.c_uint64
size = c.c_size_t
lib.dae_init.restype = handle
lib.dae_shutdown.argtypes = [handle]
lib.dae_capabilities_json.argtypes = [handle, c.c_void_p, size]
lib.dae_capabilities_json.restype = size
lib.dae_last_error.argtypes = [c.c_void_p, size]
lib.dae_last_error.restype = size
lib.dae_decode_open.argtypes = [handle, c.c_char_p]
lib.dae_decode_read.argtypes = [handle, c.POINTER(c.c_float), size, c.POINTER(size)]
lib.dae_decode_close.argtypes = [handle]


def capabilities(engine):
    while True:
        needed = lib.dae_capabilities_json(engine, None, 0)
        assert needed > 0
        buf = c.create_string_buffer(needed)
        result = lib.dae_capabilities_json(engine, buf, len(buf))
        assert result > 0
        if result <= len(buf):
            return json.loads(buf.value)


engine = lib.dae_init()
assert engine
try:
    assert not capabilities(engine)["stream"]["exclusive"]
    for filename, codec in [("tone.flac", "flac"), ("tone.m4a", "aac"), ("tone.opus", "opus")]:
        path = root / "tests" / "fixtures" / filename
        assert lib.dae_decode_open(engine, str(path).encode("utf-8")) == 0
        info = capabilities(engine)["track"]
        assert (info["codec"], info["sample_rate"], info["channels"]) == (codec, 48000, 2)
        buf = (c.c_float * 514)()
        count = size()
        frames = 0
        while True:
            assert lib.dae_decode_read(engine, buf, len(buf), c.byref(count)) == 0
            if count.value == 0:
                break
            frames += count.value
        assert frames == (4096 if codec == "aac" else 2400), (codec, frames)
        assert capabilities(engine)["track"]["measured_payload_bitrate_bps"] > 0
        assert lib.dae_decode_close(engine) == 0
finally:
    assert lib.dae_shutdown(engine) == 0
assert lib.dae_shutdown(engine) == -1
n = lib.dae_last_error(None, 0)
buf = c.create_string_buffer(n)
lib.dae_last_error(buf, n)
assert b"invalid engine handle" in buf.value
print("C ABI smoke passed: lifecycle, JSON, AAC, FLAC, Opus, stale handles")
