#pragma once

struct PerfettoTracingSession;

#include "perfetto-sdk/perfetto.h"
#include "tracing-perfetto-sdk-sys/src/lib.rs.h"
#include <cstdint>
#include <memory>
#include <rust/cxx.h>

using LogCallback = rust::Fn<void(LogLev level, int line, rust::Str filename,
                                  rust::Str message)>;

using PollTracesCallback =
    rust::Fn<void(rust::Box<PollTracesCtx> ctx, rust::Slice<const uint8_t> data,
                  bool has_more)>;

using FlushCallback = rust::Fn<void(rust::Box<FlushCtx> ctx, bool success)>;

void perfetto_global_init(LogCallback log_callback,
                          rust::Str name,
                          bool enable_in_process_backend,
                          bool enable_system_backend);

struct PerfettoTracingSession {
public:
  explicit PerfettoTracingSession(perfetto::TraceConfig trace_config,
                                  int output_fd) noexcept;

  void start() noexcept;
  void stop() noexcept;
  void flush(uint32_t timeout_ms, rust::Box<FlushCtx> ctx,
             FlushCallback done) noexcept;
  void poll_traces(rust::Box<PollTracesCtx> ctx,
                   PollTracesCallback done) noexcept;

private:
  std::unique_ptr<perfetto::TracingSession> raw_session;
};

std::unique_ptr<PerfettoTracingSession>
new_tracing_session(rust::Slice<const uint8_t> trace_config_bytes,
                    int output_fd);

void trace_track_event_slice_begin(uint64_t track_uuid, rust::Str name,
                                   rust::Str location_file,
                                   uint32_t location_line,
                                   const DebugAnnotations &debug_annotations);

void trace_track_event_slice_end(uint64_t track_uuid, rust::Str name,
                                 rust::Str location_file,
                                 uint32_t location_line);

void trace_track_event_instant(uint64_t track_uuid, rust::Str name,
                               rust::Str location_file, uint32_t location_line,
                               const DebugAnnotations &debug_annotations);

void trace_track_descriptor_process(uint64_t parent_uuid, uint64_t track_uuid,
                                    rust::Str process_name,
                                    uint32_t process_pid);

void trace_track_descriptor_thread(uint64_t parent_uuid, uint64_t track_uuid,
                                   uint32_t process_pid, rust::Str thread_name,
                                   uint32_t thread_tid);

uint64_t trace_time_ns();

uint32_t trace_clock_id();
