//! [MOC-153] transfer 注入给 Codex catalog 的顶层 `base_instructions` sentinel。
//!
//! ## 背景(2026-06-07 真机 isolated `codex exec` 实测坐实)
//!
//! Codex 用 `model_catalog_json` 里某 model 条目的 `base_instructions` 字段填充
//! 会话级 `session_meta.base_instructions`,并**在会话创建时冻结、跨 resume 不重
//! 解析**(实测:catalog 写什么字符串,session_meta 就原样冻结什么)。
//!
//! transfer 历史上给每个 catalog 条目写空串 `""`,于是第三方模型起步的会话顶层
//! instructions 一直为空。用户**停掉 transfer 后把该会话切到真 GPT** 续话时,Codex
//! 仍把这个冻结的空 instructions 发给真 ChatGPT 的 Responses 后端;后端硬校验顶层
//! instructions 非空 → 400 `Instructions are required`(MOC-153)。真 GPT 直连、
//! 不经 transfer,proxy 够不着该请求 → 唯一可行的修法是让第三方会话**出生就带非空
//! base_instructions**。
//!
//! ## 这个常量的两端用途
//!
//! - **写入**:`crates/codex_integration/src/model_catalog.rs` 把每个 catalog 条目
//!   的 `base_instructions` 写成本常量(替代旧的 `""`)。
//! - **剥离**:`crates/adapters/src/responses/request.rs` 在构造转发给第三方 chat
//!   provider 的请求时,**剥掉**等于本常量的顶层 instructions。
//!
//! 为什么要剥:本 sentinel **只**为"切真 GPT 续话"过后端校验而存在,内容只需非空且
//! 中性即可 —— 真 GPT 的人格/行为不靠它(据 MOC-153 issue 分析,Codex 切模型时把 GPT
//! 人格塞进 input 的 `<model_switch>` developer 消息;**此点引自 issue 描述、未在本仓
//! 抓包亲验**,但本修复的正确性不依赖它成立)。对第三方而言本 sentinel 纯属噪音(历史
//! 上第三方请求顶层 instructions 本就为空,仅在注册 apply_patch 的 first turn 才另有
//! adapter 注入的 chat-path 指引),剥掉后第三方请求与历史行为在**当前 Codex 的
//! instructions wire 形态(裸 string / `{ text | content }` 对象)下字节级一致、零污染**。
//!
//! 两端共用同一常量,保证 single source of truth —— 写进去的 sentinel 一定被对应
//! 版本的 adapter 精确识别并剥离。

/// transfer 注入给 Codex catalog 条目的非空 `base_instructions` sentinel。
///
/// 详见[模块文档](self)。catalog 写入端与 adapter 剥离端共用本常量。内容是一段
/// 中性的 coding-agent 系统提示:既能让"切真 GPT 续话"时顶层 instructions 非空过
/// 校验,万一被单独使用也是一段合理的兜底提示。
///
/// ⚠️ **改这个字面值是 breaking change**:sentinel 在会话创建时被冻结进
/// `session_meta`(见模块文档)。用旧值创建、用新版本 resume 的存量第三方会话,其冻结
/// 的旧 sentinel 不再 `==` 新常量 → adapter 剥不掉 → 旧 sentinel 作 system 头**静默
/// 泄漏给第三方 provider**(轻度污染、非崩溃)。若确需改值,须把旧值一并保留进
/// `is_cas_injected_base_instructions` 的剥离白名单,不能直接替换。
pub const CAS_BASE_INSTRUCTIONS: &str = "You are a coding agent operating inside the Codex CLI, collaborating with the user in their workspace. Read and edit files, run commands, and complete software-engineering tasks precisely. Make minimal, correct changes, verify your work before reporting, and follow any project- or task-specific instructions provided later in the conversation.";
