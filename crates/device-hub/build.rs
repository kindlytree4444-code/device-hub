//! 実行ファイルにアイコンを埋め込む (`.rsrc` が無いとエクスプローラーもタスクバーも既定の絵になる)。
//! `gpui-pre-windows` の `load_icon` は自分自身のリソース ID 1 を読むので、窓のアイコンも
//! これで同時に付く。中身は `build-support/icon.rs` (参照実装と同じもの)。

#[path = "build-support/icon.rs"]
mod icon;

fn main() {
    println!("cargo:rerun-if-changed=build-support/icon.rs");
    icon::embed("assets/icon.ico");
    // GPUI の大きなビュー関数はデバッグビルドで既定の 1MB スタックを溢れ得る
    // (参照実装で設定ページの `stack overflow` 実績あり)。メインスレッド予約を広げる。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let stack = if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
            "-Wl,--stack,16777216"
        } else {
            "/STACK:16777216"
        };
        println!("cargo:rustc-link-arg-bin=device-hub={stack}");
    }
}
