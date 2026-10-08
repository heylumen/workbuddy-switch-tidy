fn main() {
    // 本 fork：声明监听图标资源，改动后才会重新编译并重新嵌入（上游默认不监听 icons/）
    println!("cargo:rerun-if-changed=icons/icon.ico");
    println!("cargo:rerun-if-changed=icons/icon-windows.png");
    println!("cargo:rerun-if-changed=icons/tray-icon-color.rgba");

    tauri_build::build()
}
