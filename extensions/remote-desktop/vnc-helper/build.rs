use std::env;
use std::path::PathBuf;

fn main() {
    // 只构建 libvncclient，关掉所有示例（避免 Qt5/SDL/GTK/FFMPEG 例子编译失败）。
    let dst = cmake::Config::new("libvncserver")
        .define("WITH_EXAMPLES", "OFF")
        .define("WITH_QT", "OFF")
        .define("WITH_SDL", "OFF")
        .define("WITH_GTK", "OFF")
        .define("WITH_FFMPEG", "OFF")
        .define("WITH_LIBVNCSERVER", "OFF") // 只要客户端库
        .define("WITH_LIBVNCCLIENT", "ON")
        .define("BUILD_SHARED_LIBS", "OFF")
        // crypto：OpenSSL 支撑 TLS(18)；ARD(30) 的 DH/AES 为内置实现，不依赖此项
        .define("WITH_OPENSSL", "ON")
        .define("WITH_GNUTLS", "OFF")
        .define("WITH_GCRYPT", "OFF")
        .build_target("vncclient")
        .build();

    // 静态链接 vncclient 及其依赖（库在 build 目录，头文件在源码 include + 生成的 build/include）
    let build_dir = dst.join("build");
    println!("cargo:rustc-link-search={}", build_dir.display());
    println!("cargo:rustc-link-lib=static=vncclient");
    println!("cargo:rustc-link-lib=z");
    println!("cargo:rustc-link-lib=jpeg");
    println!("cargo:rustc-link-lib=pthread");
    // TLS/加密后端（WITH_OPENSSL=ON 时必需）
    println!("cargo:rustc-link-lib=ssl");
    println!("cargo:rustc-link-lib=crypto");

    let src_include = PathBuf::from("libvncserver/include");
    let gen_include = build_dir.join("include");
    let bindings = bindgen::Builder::default()
        .header(
            src_include
                .join("rfb/rfbclient.h")
                .to_string_lossy()
                .into_owned(),
        )
        .clang_arg(format!("-I{}", src_include.display()))
        .clang_arg(format!("-I{}", gen_include.display()))
        .use_core()
        .allowlist_function("rfb.*")
        .allowlist_function("Send.*")
        .allowlist_function("WaitForMessage")
        .allowlist_function("HandleRFBServerMessage")
        .allowlist_function("malloc")
        .allowlist_type("rfb.*")
        .allowlist_type("_rfbClient")
        .allowlist_var("rfb.*")
        .generate()
        .expect("无法生成 rfb 绑定");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("rfb.rs"))
        .expect("无法写入绑定");

    // vendored 源码变化时重建
    println!("cargo:rerun-if-changed=libvncserver/include/rfb/rfbclient.h");
}
