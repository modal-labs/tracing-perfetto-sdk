#include "perfetto-bindings.h"
#include <stdexcept>

class RustTracingDataSource
    : public perfetto::DataSource<RustTracingDataSource> {
public:
  // Empty implementations of these purely virtual methods:
  void OnSetup(const SetupArgs &) override {}

  void OnStart(const StartArgs &) override {}

  void OnStop(const StopArgs &) override {}
};

PERFETTO_DECLARE_DATA_SOURCE_STATIC_MEMBERS(RustTracingDataSource);
PERFETTO_DEFINE_DATA_SOURCE_STATIC_MEMBERS(RustTracingDataSource);

PERFETTO_DEFINE_CATEGORIES();
PERFETTO_TRACK_EVENT_STATIC_STORAGE();

static LogCallback _g_log_callback;

static void _log_callback_wrapper(perfetto::LogMessageCallbackArgs args) {
  rust::Str filename{args.filename};
  rust::Str message{args.message};
  _g_log_callback((LogLev)args.level, args.line, filename, message);
}

void perfetto_global_init(LogCallback log_callback,
                          rust::Str name,
                          bool enable_in_process_backend,
                          bool enable_system_backend) {
  perfetto::TracingInitArgs args;

  if (enable_in_process_backend) {
    args.backends |= perfetto::kInProcessBackend;
  }
  if (enable_system_backend) {
    args.backends |= perfetto::kSystemBackend;
  }

  _g_log_callback = log_callback;
  args.log_message_callback = _log_callback_wrapper;

  perfetto::Tracing::Initialize(args);

  // Register TrackEvent data source
  perfetto::TrackEvent::Register();

  // Register Rust tracing support as a custom data source
  perfetto::DataSourceDescriptor dsd;
  dsd.set_name(std::string(name));
  RustTracingDataSource::Register(dsd);
}

PerfettoTracingSession::PerfettoTracingSession(
    perfetto::TraceConfig trace_config, int output_fd) noexcept
    : raw_session(perfetto::Tracing::NewTrace()) {
  this->raw_session->Setup(trace_config, output_fd);
}

void PerfettoTracingSession::start() noexcept {
  this->raw_session->StartBlocking();
}

void PerfettoTracingSession::stop() noexcept {
  this->raw_session->StopBlocking();
}

void PerfettoTracingSession::flush(uint32_t timeout_ms, rust::Box<FlushCtx> ctx,
                                   FlushCallback done) noexcept {
  // Need to make a shared_ptr here because even though we know the
  // lambda passed to ReadTrace will only be called once, the compiler does not,
  // so we need something copyable that we can move out of
  std::shared_ptr<rust::Box<FlushCtx>> shared_ctx =
      std::make_shared<rust::Box<FlushCtx>>(std::move(ctx));

  this->raw_session->Flush(
      [shared_ctx = std::move(shared_ctx), done](bool success) mutable {
        auto ptr = shared_ctx->into_raw();
        if (ptr) {
          rust::Box<FlushCtx> ctx = rust::Box<FlushCtx>::from_raw(ptr);
          done(std::move(ctx), success);
        }
      },
      timeout_ms);
}

void PerfettoTracingSession::poll_traces(rust::Box<PollTracesCtx> ctx,
                                         PollTracesCallback done) noexcept {
  // Need to make a shared_ptr here because even though we know the
  // lambda passed to ReadTrace will only be called once, the compiler does not,
  // so we need something copyable that we can move out of
  std::shared_ptr<rust::Box<PollTracesCtx>> shared_ctx =
      std::make_shared<rust::Box<PollTracesCtx>>(std::move(ctx));

  this->raw_session->ReadTrace(
      [=](perfetto::TracingSession::ReadTraceCallbackArgs args) mutable {
        auto ptr = shared_ctx->into_raw();
        if (ptr) {
          rust::Box<PollTracesCtx> ctx =
              rust::Box<PollTracesCtx>::from_raw(ptr);
          if (args.data && args.size) {
            auto data_ptr = reinterpret_cast<const uint8_t *>(args.data);
            rust::Slice<const uint8_t> data{data_ptr, args.size};
            done(std::move(ctx), data, args.has_more);
          } else {
            rust::Slice<const uint8_t> data; // empty slice
            done(std::move(ctx), data, args.has_more);
          }
        }
      });
}

std::unique_ptr<PerfettoTracingSession>
new_tracing_session(rust::Slice<const uint8_t> trace_config_bytes,
                    int output_fd) {
  perfetto::TraceConfig trace_config;
  if (!perfetto::Tracing::IsInitialized()) {
    throw std::runtime_error(
        "Must call perfetto_global_init before creating a tracing session");
  } else if (trace_config.ParseFromArray(trace_config_bytes.data(),
                                         trace_config_bytes.size())) {
    return std::make_unique<PerfettoTracingSession>(trace_config, output_fd);
  } else {
    throw std::invalid_argument("trace_config_bytes didn't contain a valid "
                                "perfetto::TraceConfig proto message");
  }
}

static void
_add_debug_annotations(perfetto::protos::pbzero::TrackEvent *track_event,
                       const DebugAnnotations &debug_annotations) {
  for (auto it = debug_annotations.strings.begin();
       it != debug_annotations.strings.end(); it++) {
    auto ann = track_event->add_debug_annotations();
    ann->set_name(it->key.data(), it->key.size());
    ann->set_string_value(it->value.data(), it->value.size());
  }
  for (auto it = debug_annotations.bools.begin();
       it != debug_annotations.bools.end(); it++) {
    auto ann = track_event->add_debug_annotations();
    ann->set_name(it->key.data(), it->key.size());
    ann->set_bool_value(it->value);
  }
  for (auto it = debug_annotations.ints.begin();
       it != debug_annotations.ints.end(); it++) {
    auto ann = track_event->add_debug_annotations();
    ann->set_name(it->key.data(), it->key.size());
    ann->set_int_value(it->value);
  }
  for (auto it = debug_annotations.doubles.begin();
       it != debug_annotations.doubles.end(); it++) {
    auto ann = track_event->add_debug_annotations();
    ann->set_name(it->key.data(), it->key.size());
    ann->set_double_value(it->value);
  }
}

void trace_track_event_slice_begin(uint64_t track_uuid, rust::Str name,
                                   rust::Str location_file,
                                   uint32_t location_line,
                                   const DebugAnnotations &debug_annotations) {
  RustTracingDataSource::Trace([&](RustTracingDataSource::TraceContext ctx) {
    auto packet = ctx.NewTracePacket();
    packet->set_timestamp(perfetto::TrackEvent::GetTraceTimeNs());

    auto track_event = packet->set_track_event();
    track_event->set_type(
        perfetto::protos::pbzero::perfetto_pbzero_enum_TrackEvent::
            TYPE_SLICE_BEGIN);
    if (track_uuid) {
      track_event->set_track_uuid(track_uuid);
    }
    if (!name.empty()) {
      track_event->set_name(name.data(), name.size());
    }
    _add_debug_annotations(track_event, debug_annotations);

    if (!location_file.empty()) {
      auto source_location = track_event->set_source_location();
      source_location->set_file_name(location_file.data(),
                                     location_file.size());
      if (location_line) {
        source_location->set_line_number(location_line);
      }
    }
  });
}

void trace_track_event_slice_end(uint64_t track_uuid, rust::Str name,
                                 rust::Str location_file,
                                 uint32_t location_line) {
  RustTracingDataSource::Trace([&](RustTracingDataSource::TraceContext ctx) {
    auto packet = ctx.NewTracePacket();
    packet->set_timestamp(perfetto::TrackEvent::GetTraceTimeNs());

    auto track_event = packet->set_track_event();
    track_event->set_type(perfetto::protos::pbzero::
                              perfetto_pbzero_enum_TrackEvent::TYPE_SLICE_END);
    if (track_uuid) {
      track_event->set_track_uuid(track_uuid);
    }
    if (!name.empty()) {
      track_event->set_name(name.data(), name.size());
    }

    if (!location_file.empty()) {
      auto source_location = track_event->set_source_location();
      source_location->set_file_name(location_file.data(),
                                     location_file.size());
      if (location_line) {
        source_location->set_line_number(location_line);
      }
    }
  });
}

void trace_track_event_instant(uint64_t track_uuid, rust::Str name,
                               rust::Str location_file, uint32_t location_line,
                               const DebugAnnotations &debug_annotations) {
  RustTracingDataSource::Trace([&](RustTracingDataSource::TraceContext ctx) {
    auto packet = ctx.NewTracePacket();
    packet->set_timestamp(perfetto::TrackEvent::GetTraceTimeNs());

    auto track_event = packet->set_track_event();
    track_event->set_type(perfetto::protos::pbzero::
                              perfetto_pbzero_enum_TrackEvent::TYPE_INSTANT);
    if (track_uuid) {
      track_event->set_track_uuid(track_uuid);
    }
    if (!name.empty()) {
      track_event->set_name(name.data(), name.size());
    }
    _add_debug_annotations(track_event, debug_annotations);

    if (!location_file.empty()) {
      auto source_location = track_event->set_source_location();
      source_location->set_file_name(location_file.data(),
                                     location_file.size());
      if (location_line) {
        source_location->set_line_number(location_line);
      }
    }
  });
}

void trace_track_descriptor_process(uint64_t parent_uuid, uint64_t track_uuid,
                                    rust::Str process_name,
                                    uint32_t process_pid) {
  RustTracingDataSource::Trace([&](RustTracingDataSource::TraceContext ctx) {
    auto packet = ctx.NewTracePacket();
    packet->set_timestamp(perfetto::TrackEvent::GetTraceTimeNs());

    auto track_descriptor = packet->set_track_descriptor();
    if (track_uuid) {
      track_descriptor->set_uuid(track_uuid);
    }
    if (parent_uuid) {
      track_descriptor->set_parent_uuid(parent_uuid);
    }
    if (!process_name.empty()) {
      track_descriptor->set_name(process_name.data(), process_name.size());
    }

    auto process = track_descriptor->set_process();
    if (process_pid) {
      process->set_pid(process_pid);
    }
    if (!process_name.empty()) {
      process->set_process_name(process_name.data(), process_name.size());
    }
  });
}

void trace_track_descriptor_thread(uint64_t parent_uuid, uint64_t track_uuid,
                                   uint32_t process_pid, rust::Str thread_name,
                                   uint32_t thread_tid) {
  RustTracingDataSource::Trace([&](RustTracingDataSource::TraceContext ctx) {
    auto packet = ctx.NewTracePacket();
    packet->set_timestamp(perfetto::TrackEvent::GetTraceTimeNs());

    auto track_descriptor = packet->set_track_descriptor();
    if (track_uuid) {
      track_descriptor->set_uuid(track_uuid);
    }
    if (parent_uuid) {
      track_descriptor->set_parent_uuid(parent_uuid);
    }
    if (!thread_name.empty()) {
      track_descriptor->set_name(thread_name.data(), thread_name.size());
    }

    auto thread = track_descriptor->set_thread();
    if (process_pid) {
      thread->set_pid(process_pid);
    }
    if (thread_tid) {
      thread->set_tid(thread_tid);
    }
    if (!thread_name.empty()) {
      thread->set_thread_name(thread_name.data(), thread_name.size());
    }
  });
}

uint64_t trace_time_ns() { return perfetto::TrackEvent::GetTraceTimeNs(); }

uint32_t trace_clock_id() { return perfetto::TrackEvent::GetTraceClockId(); }
