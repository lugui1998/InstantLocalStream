use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=ILS_FFMPEG_PATH");
    println!("cargo:rerun-if-env-changed=ILS_FFMPEG_LICENSE_PATH");
    println!("cargo:rerun-if-changed=web");
    println!("cargo:rerun-if-changed=packaging/windows/Instant-Local-Stream.rc");
    println!("cargo:rerun-if-changed=packaging/windows/Instant-Local-Stream.ico");

    embed_resource::compile(
        "packaging/windows/Instant-Local-Stream.rc",
        embed_resource::NONE,
    )
    .manifest_required()
    .expect("embed Windows application icon");

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
    let has_embedded_ffmpeg = source.is_some();
    let ffmpeg = match source {
        Some(path) => format!(
            "pub const EMBEDDED_FFMPEG: Option<&[u8]> = Some(include_bytes!(r#\"{}\"#));\n",
            path.display()
        ),
        None => "pub const EMBEDDED_FFMPEG: Option<&[u8]> = None;\n".to_owned(),
    };
    let license_source = env::var_os("ILS_FFMPEG_LICENSE_PATH")
        .map(PathBuf::from)
        .filter(|path| {
            path.is_file()
                && fs::metadata(path)
                    .map(|metadata| metadata.len() > 0)
                    .unwrap_or(false)
        });
    if has_embedded_ffmpeg && license_source.is_none() {
        panic!(
            "ILS_FFMPEG_LICENSE_PATH must point to the matching FFmpeg license when embedding FFmpeg"
        );
    }
    let license = license_source
        .map(|path| {
            format!(
                "pub const EMBEDDED_FFMPEG_LICENSE: Option<&[u8]> = Some(include_bytes!(r#\"{}\"#));\n",
                path.display()
            )
        })
        .unwrap_or_else(|| {
            "pub const EMBEDDED_FFMPEG_LICENSE: Option<&[u8]> = None;\n".to_owned()
        });
    let contents = format!("{ffmpeg}{license}");
    fs::write(generated, contents).expect("write generated FFmpeg module");
}
