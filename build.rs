use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=ILS_FFMPEG_PATH");
    println!("cargo:rerun-if-changed=web");
    if !PathBuf::from("web/dist/index.html").is_file() {
        panic!(
            "web/dist/index.html is missing; run scripts/build-web.ps1 or scripts/build-web.sh before building the Rust application"
        );
    }
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let generated = out_dir.join("embedded_ffmpeg.rs");
    let source = env::var_os("ILS_FFMPEG_PATH")
        .map(PathBuf::from)
        .filter(|path| {
            path.is_file()
                && fs::metadata(path)
                    .map(|metadata| metadata.len() > 0)
                    .unwrap_or(false)
        });
    let contents = match source {
        Some(path) => format!(
            "pub const EMBEDDED_FFMPEG: Option<&[u8]> = Some(include_bytes!(r#\"{}\"#));\n",
            path.display()
        ),
        None => "pub const EMBEDDED_FFMPEG: Option<&[u8]> = None;\n".to_owned(),
    };
    fs::write(generated, contents).expect("write generated FFmpeg module");
}
