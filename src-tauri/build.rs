fn main() {
    // **关键**:`tauri_build::build()` 默认不把 ../frontend 加进 cargo rerun-if-changed,
    // 导致前端代码改了 binary 不重 build。实测 2026-05-10 用户截图显示前端协议名 fallback
    // 错误显示 "OpenAI Chat" 而不是 "Gemini Native",root cause 就是 binary 用的是
    // 5月9日的旧版本(cargo 没探测到 frontend 改动)。
    //
    // 显式声明 frontend 改动 + presets_data.json(embed 进 binary)→ 触发 rerun build。
    println!("cargo:rerun-if-changed=../frontend");
    println!("cargo:rerun-if-changed=../crates/registry/src/presets_data.json");
    tauri_build::build()
}
