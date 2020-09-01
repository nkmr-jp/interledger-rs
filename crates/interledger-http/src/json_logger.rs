use once_cell::sync::Lazy;
use slog::{PushFnValue, *};
use std::fs::OpenOptions;
use std::sync::Mutex;

#[derive(Debug)]
pub struct Logging {
    pub logger: slog::Logger,
}

pub static LOGGING: Lazy<Logging> = Lazy::new(|| {
    let pid=std::process::id().to_string();
    let logfile = format!("../../json_logs/ilp-node-{}.log", pid);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(true)
        .open(logfile)
        .unwrap();

    let drain = slog_json::Json::new(file)
        .set_pretty(false)
        .add_default_keys()
        .add_key_value(o!(
                "pid" => FnValue(move |_|{std::process::id().to_string()})
                ))
        .build()
        .fuse();
    let applogger = Logger::root(
        Mutex::new(drain).fuse(),
        o!("location" => PushFnValue(|r: &Record, ser: PushFnValueSerializer| {
            ser.emit(format_args!("{}:{}", r.file(), r.line()))
        })),
    );

    Logging { logger: applogger }
});