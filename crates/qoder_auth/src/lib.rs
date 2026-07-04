//! QoderWork CN 的 **Cosy 签名协议** Rust 绑定。
//!
//! QoderWork 的模型请求鉴权(legacy 通道 `POST gateway.qoder.com.cn/algo/api/v2/
//! service/pro/sse/agent_chat_generation`)由官方 **Rust WASM 模块 `qoder_auth_wasm`**
//! (wasm-bindgen,JS target)生成:签名 `Authorization: Bearer COSY.<sig>` + 一整套
//! `Cosy-*` 头 + AES-GCM 加密的请求体。签名 secret / RSA 公钥 / salt 全部静态内嵌在
//! WASM 里。逆向实证与端到端验证(HTTP 200)见 Linear **MOC-297**。
//!
//! transfer 用户模型威胁下**无法在纯 Rust 复刻**该私有签名(涉及内嵌 secret + 自定义
//! canonicalization),故**内嵌官方 `.wasm`(`assets/qoder_auth_wasm_bg.wasm`,289KB)**
//! 并用 wasmtime 驱动 —— 在 Rust 侧复刻 wasm-bindgen 的 31 个 host import + 内存
//! marshaling(本文件 [`imports`] 模块)。这些 import 实现逐条对照官方 glue
//! (`qoder-worker-runtime.obf.mjs`)与本仓 POC(见 MOC-297)得到。
//!
//! ## 调用链(对齐官方 SDK)
//! ```text
//! QoderAuthContext::new(machine_id, cosy_version, user_info_json, client_meta_json)
//!   .refresh_auth_fields(user_info_for_auth_json)   // 喂 device token
//!   .prepare_infer_request(endpoint, body, key, source) -> PreparedRequest
//!       { url(带 query), headers(全套 Cosy 签名头), body(加密) }
//! ```
//! 响应侧用 [`QoderAuthContext::decrypt_server_response`] 解密 SSE 帧。
//!
//! **线程安全**:WASM 单实例非线程安全(内部可变 + 借用检查),[`QoderAuth`] 用
//! `Mutex` 串行化所有调用。

use std::sync::Mutex;

use wasmi::{Caller, Engine, Instance, Linker, Memory, Module, Store, TypedFunc};

/// 内嵌的官方 WASM。致谢:阿里 Qoder(`qoder_auth_wasm`,提取自 QoderWork CN app)。
static WASM_BYTES: &[u8] = include_bytes!("../assets/qoder_auth_wasm_bg.wasm");

/// wasm-bindgen JS-object 堆的静态槽位数(`undefined/null/true/false` 之前的填充)。
/// 官方 glue 用 128,必须一致(WASM 硬编码用 idx 128..=131 表示这四个常量)。
const HEAP_BASE: u32 = 128;

#[derive(Debug, thiserror::Error)]
pub enum QoderAuthError {
    #[error("WASM 引擎初始化失败: {0}")]
    Engine(String),
    #[error("WASM 调用失败: {0}")]
    Wasm(String),
    #[error("WASM 侧抛错: {0}")]
    Trap(String),
    #[error("UTF-8 解码失败: {0}")]
    Utf8(#[from] std::str::Utf8Error),
    #[error("内存越界: ptr={ptr} len={len}")]
    Oob { ptr: u32, len: u32 },
}

type Result<T> = std::result::Result<T, QoderAuthError>;

// ── wasm-bindgen JS-object 堆(host 侧复刻)──────────────────────────

/// 堆里的 JS 值。只覆盖本 WASM 实际用到的类型。
enum HeapVal {
    /// free-list:该槽空闲,存下一个空闲槽 idx。
    Free(u32),
    Undefined,
    Null,
    Bool(bool),
    /// `new Uint8Array(n)` —— 独立 owned buffer(非 WASM 内存视图)。
    Bytes(Vec<u8>),
    /// WASM 内存视图 `(ptr, len)` —— getRandomValues 填充 / subarray / setcall 目标。
    MemView {
        ptr: u32,
        len: u32,
    },
    Str(String),
    /// `new Map()` —— headers 累积;用 `Rc<RefCell>` 让多个 heap ref 共享同一 map
    /// (WASM 会 clone_ref 后继续 set,身份必须一致)。
    Map(std::rc::Rc<std::cell::RefCell<Vec<(String, String)>>>),
    /// `globalThis.crypto` 标记。
    Crypto,
    /// `globalThis` 标记。
    Global,
}

/// wasmtime `Store` 的 host 数据:堆 + `WASM_VECTOR_LEN` + 最近一次 WASM 抛错。
struct HostState {
    heap: Vec<HeapVal>,
    heap_next: u32,
    /// `passStr` 写入后的字节长度(对齐官方 glue 的全局 `WASM_VECTOR_LEN`）。
    vlen: u32,
    /// `cTA` catch 分支存下的 WASM 侧错误(`__wbindgen_export`),读出后即 take。
    last_err: Option<String>,
}

impl HostState {
    fn new() -> Self {
        // heap = [Undefined;128] + Undefined + Null + true + false;heap_next=132。
        let mut heap = Vec::with_capacity((HEAP_BASE + 8) as usize);
        for _ in 0..HEAP_BASE {
            heap.push(HeapVal::Undefined);
        }
        heap.push(HeapVal::Undefined); // 128
        heap.push(HeapVal::Null); // 129
        heap.push(HeapVal::Bool(true)); // 130
        heap.push(HeapVal::Bool(false)); // 131
        HostState {
            heap_next: HEAP_BASE + 4,
            heap,
            vlen: 0,
            last_err: None,
        }
    }

    fn add(&mut self, v: HeapVal) -> u32 {
        if self.heap_next as usize == self.heap.len() {
            let next = self.heap.len() as u32 + 1;
            self.heap.push(HeapVal::Free(next));
        }
        let idx = self.heap_next;
        self.heap_next = match &self.heap[idx as usize] {
            HeapVal::Free(n) => *n,
            _ => idx + 1,
        };
        self.heap[idx as usize] = v;
        idx
    }

    fn get(&self, idx: u32) -> &HeapVal {
        &self.heap[idx as usize]
    }

    fn drop_ref(&mut self, idx: u32) {
        if idx < HEAP_BASE + 4 {
            return; // 静态常量槽不回收
        }
        self.heap[idx as usize] = HeapVal::Free(self.heap_next);
        self.heap_next = idx;
    }

    fn take(&mut self, idx: u32) -> HeapVal {
        let v = std::mem::replace(&mut self.heap[idx as usize], HeapVal::Undefined);
        self.drop_ref(idx);
        v
    }

    /// clone 一个 heap 值到新槽(`__wbindgen_object_clone_ref`)。
    fn clone_ref(&mut self, idx: u32) -> u32 {
        let v = match self.get(idx) {
            HeapVal::Undefined => HeapVal::Undefined,
            HeapVal::Null => HeapVal::Null,
            HeapVal::Bool(b) => HeapVal::Bool(*b),
            HeapVal::Bytes(b) => HeapVal::Bytes(b.clone()),
            HeapVal::MemView { ptr, len } => HeapVal::MemView {
                ptr: *ptr,
                len: *len,
            },
            HeapVal::Str(s) => HeapVal::Str(s.clone()),
            HeapVal::Map(m) => HeapVal::Map(m.clone()), // Rc 共享同一 map
            HeapVal::Crypto => HeapVal::Crypto,
            HeapVal::Global => HeapVal::Global,
            HeapVal::Free(_) => HeapVal::Undefined,
        };
        self.add(v)
    }
}

// ── 内存读写 helper ────────────────────────────────────────────────

fn read_u32(data: &[u8], off: u32) -> u32 {
    let o = off as usize;
    u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]])
}

fn read_i32(data: &[u8], off: u32) -> i32 {
    read_u32(data, off) as i32
}

fn write_bytes(data: &mut [u8], ptr: u32, src: &[u8]) {
    let p = ptr as usize;
    data[p..p + src.len()].copy_from_slice(src);
}

fn mem_slice(data: &[u8], ptr: u32, len: u32) -> Option<&[u8]> {
    let (p, l) = (ptr as usize, len as usize);
    data.get(p..p + l)
}

/// 从 caller 取 WASM 线性内存 + host 数据(同时可变借用)。
fn mem_and_state<'a>(caller: &'a mut Caller<'_, HostState>) -> (&'a mut [u8], &'a mut HostState) {
    let mem = caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .expect("WASM 必须导出 memory");
    mem.data_and_store_mut(caller)
}

// ── 31 个 host import(逐条对照官方 glue + POC)──────────────────────

mod imports {
    use super::*;

    /// 把全部 import 注册到 linker 的 `./qoder_auth_wasm_bg.js` 命名空间。
    pub(super) fn register(linker: &mut Linker<HostState>) -> Result<()> {
        let ns = "./qoder_auth_wasm_bg.js";
        macro_rules! wrap {
            ($name:expr, $f:expr) => {
                linker
                    .func_wrap(ns, $name, $f)
                    .map_err(|e| QoderAuthError::Engine(e.to_string()))?;
            };
        }

        // 对象生命周期
        wrap!(
            "__wbindgen_object_drop_ref",
            |mut c: Caller<'_, HostState>, i: i32| {
                c.data_mut().take(i as u32);
            }
        );
        wrap!("__wbindgen_object_clone_ref", |mut c: Caller<
            '_,
            HostState,
        >,
                                              i: i32|
         -> i32 {
            c.data_mut().clone_ref(i as u32) as i32
        });

        // crypto 检测:强制 browser 路径(globalThis.crypto.getRandomValues)
        wrap!("__wbg_crypto_38df2bab126b63dc", |mut c: Caller<
            '_,
            HostState,
        >,
                                                _i: i32|
         -> i32 {
            c.data_mut().add(HeapVal::Crypto) as i32
        });
        wrap!("__wbg_process_44c7a14e11e9f69e", |mut c: Caller<
            '_,
            HostState,
        >,
                                                 _i: i32|
         -> i32 {
            c.data_mut().add(HeapVal::Undefined) as i32
        });
        wrap!("__wbg_versions_276b2795b1c6a219", |mut c: Caller<
            '_,
            HostState,
        >,
                                                  _i: i32|
         -> i32 {
            c.data_mut().add(HeapVal::Undefined) as i32
        });
        wrap!("__wbg_node_84ea875411254db1", |mut c: Caller<
            '_,
            HostState,
        >,
                                              _i: i32|
         -> i32 {
            c.data_mut().add(HeapVal::Undefined) as i32
        });
        wrap!("__wbg_require_b4edbdcf3e2a1ef0", |mut c: Caller<
            '_,
            HostState,
        >|
         -> i32 {
            c.data_mut().add(HeapVal::Undefined) as i32
        });
        wrap!("__wbg_msCrypto_bd5a034af96bcba6", |mut c: Caller<
            '_,
            HostState,
        >,
                                                  _i: i32|
         -> i32 {
            c.data_mut().add(HeapVal::Undefined) as i32
        });

        // getRandomValues(ptr,len):填 WASM 内存视图
        wrap!(
            "__wbg_getRandomValues_d49329ff89a07af1",
            |mut c: Caller<'_, HostState>, ptr: i32, len: i32| {
                let (data, _s) = mem_and_state(&mut c);
                let (p, l) = (ptr as usize, len as usize);
                let _ = getrandom::getrandom(&mut data[p..p + l]);
            }
        );
        // getRandomValues(cryptoObj, bufferObj):buffer 是 heap 对象
        wrap!(
            "__wbg_getRandomValues_c44a50d8cfdaebeb",
            |mut c: Caller<'_, HostState>, _crypto: i32, buf: i32| {
                fill_heap_buffer(&mut c, buf as u32);
            }
        );
        // randomFillSync(cryptoObj, bufferObj):node 路径(browser 路径下不会被调,兜底填充)
        wrap!(
            "__wbg_randomFillSync_6c25eac9869eb53c",
            |mut c: Caller<'_, HostState>, _crypto: i32, buf: i32| {
                let b = c.data_mut().take(buf as u32);
                let idx = c.data_mut().add(b);
                fill_heap_buffer(&mut c, idx);
                c.data_mut().take(idx);
            }
        );

        // Map.set(map,k,v) -> map;typed-array set 也复用此 import 名 → 按对象类型分派
        wrap!("__wbg_set_08463b1df38a7e29", |mut c: Caller<
            '_,
            HostState,
        >,
                                             a: i32,
                                             i: i32,
                                             big: i32|
         -> i32 {
            let s = c.data_mut();
            if let HeapVal::Map(m) = s.get(a as u32) {
                let m = m.clone();
                let k = heapval_to_string(s.get(i as u32));
                let v = heapval_to_string(s.get(big as u32));
                m.borrow_mut().push((k, v));
            }
            s.clone_ref(a as u32) as i32
        });

        // Function.call(thisArg, arg) —— (fn, this, arg)。本 WASM 里极少用,回 undefined。
        wrap!("__wbg_call_d578befcc3145dee", |mut c: Caller<
            '_,
            HostState,
        >,
                                              _f: i32,
                                              _this: i32,
                                              _arg: i32|
         -> i32 {
            c.data_mut().add(HeapVal::Undefined) as i32
        });

        wrap!("__wbg_new_with_length_9cedd08484b73942", |mut c: Caller<
            '_,
            HostState,
        >,
                                                         n: i32|
         -> i32 {
            c.data_mut()
                .add(HeapVal::Bytes(vec![0u8; n as u32 as usize])) as i32
        });
        wrap!("__wbg_length_0c32cb8543c8e4c8", |c: Caller<
            '_,
            HostState,
        >,
                                                i: i32|
         -> i32 {
            match c.data().get(i as u32) {
                HeapVal::Bytes(b) => b.len() as i32,
                HeapVal::MemView { len, .. } => *len as i32,
                HeapVal::Str(s) => s.len() as i32,
                _ => 0,
            }
        });
        // Uint8Array.prototype.set.call(view(ptr,len), src):src 拷进 WASM 内存
        wrap!(
            "__wbg_prototypesetcall_3e05eb9545565046",
            |mut c: Caller<'_, HostState>, ptr: i32, len: i32, src: i32| {
                let (data, state) = mem_and_state(&mut c);
                let bytes: Vec<u8> = match state.get(src as u32) {
                    HeapVal::Bytes(b) => b.clone(),
                    HeapVal::MemView { ptr: sp, len: sl } => mem_slice(data, *sp, *sl)
                        .map(|s| s.to_vec())
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                let n = (len as usize).min(bytes.len());
                write_bytes(data, ptr as u32, &bytes[..n]);
            }
        );
        // subarray(obj,start,end) -> 子视图
        wrap!("__wbg_subarray_0f98d3fb634508ad", |mut c: Caller<
            '_,
            HostState,
        >,
                                                  i: i32,
                                                  start: i32,
                                                  end: i32|
         -> i32 {
            let s = c.data_mut();
            let v = match s.get(i as u32) {
                HeapVal::MemView { ptr, .. } => HeapVal::MemView {
                    ptr: *ptr + start as u32,
                    len: (end - start) as u32,
                },
                HeapVal::Bytes(b) => HeapVal::Bytes(b[start as usize..end as usize].to_vec()),
                _ => HeapVal::Undefined,
            };
            s.add(v) as i32
        });
        // new Map() —— 0 参
        wrap!("__wbg_new_99cabae501c0a8a0", |mut c: Caller<
            '_,
            HostState,
        >|
         -> i32 {
            c.data_mut()
                .add(HeapVal::Map(std::rc::Rc::new(std::cell::RefCell::new(
                    Vec::new(),
                )))) as i32
        });
        // Date.now()
        wrap!(
            "__wbg_now_88621c9c9a4f3ffc",
            |_c: Caller<'_, HostState>| -> f64 {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as f64)
                    .unwrap_or(0.0)
            }
        );

        // 全局访问器 —— 一律回 Global(getRandomValues_d49329ff 走 globalThis.crypto)
        for name in [
            "__wbg_static_accessor_GLOBAL_THIS_a1248013d790bf5f",
            "__wbg_static_accessor_SELF_24f78b6d23f286ea",
            "__wbg_static_accessor_GLOBAL_f2e0f995a21329ff",
            "__wbg_static_accessor_WINDOW_59fd959c540fe405",
        ] {
            wrap!(name, |mut c: Caller<'_, HostState>| -> i32 {
                c.data_mut().add(HeapVal::Global) as i32
            });
        }

        // throw / Error:把消息记进 last_err 并 trap
        wrap!(
            "__wbg___wbindgen_throw_81fc77679af83bc6",
            |mut c: Caller<'_, HostState>, ptr: i32, len: i32| {
                let (data, state) = mem_and_state(&mut c);
                let msg = mem_slice(data, ptr as u32, len as u32)
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .unwrap_or("<throw>")
                    .to_string();
                state.last_err = Some(msg);
            }
        );
        wrap!("__wbg_Error_2e59b1b37a9a34c3", |mut c: Caller<
            '_,
            HostState,
        >,
                                               ptr: i32,
                                               len: i32|
         -> i32 {
            let (data, state) = mem_and_state(&mut c);
            let msg = mem_slice(data, ptr as u32, len as u32)
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("")
                .to_string();
            state.add(HeapVal::Str(msg)) as i32
        });

        // 类型判定
        wrap!(
            "__wbg___wbindgen_is_object_40c5a80572e8f9d3",
            |c: Caller<'_, HostState>, i: i32| -> i32 {
                matches!(
                    c.data().get(i as u32),
                    HeapVal::Map(_) | HeapVal::Crypto | HeapVal::Global
                ) as i32
            }
        );
        wrap!(
            "__wbg___wbindgen_is_string_b29b5c5a8065ba1a",
            |c: Caller<'_, HostState>, i: i32| -> i32 {
                matches!(c.data().get(i as u32), HeapVal::Str(_)) as i32
            }
        );
        wrap!(
            "__wbg___wbindgen_is_function_49868bde5eb1e745",
            |_c: Caller<'_, HostState>, _i: i32| -> i32 { 0 }
        );
        wrap!(
            "__wbg___wbindgen_is_undefined_c0cca72b82b86f4d",
            |c: Caller<'_, HostState>, i: i32| -> i32 {
                matches!(c.data().get(i as u32), HeapVal::Undefined) as i32
            }
        );

        // cast_1(ptr,len) -> Uint8Array 视图;cast_2(ptr,len) -> String
        wrap!("__wbindgen_cast_0000000000000001", |mut c: Caller<
            '_,
            HostState,
        >,
                                                   ptr: i32,
                                                   len: i32|
         -> i32 {
            c.data_mut().add(HeapVal::MemView {
                ptr: ptr as u32,
                len: len as u32,
            }) as i32
        });
        wrap!("__wbindgen_cast_0000000000000002", |mut c: Caller<
            '_,
            HostState,
        >,
                                                   ptr: i32,
                                                   len: i32|
         -> i32 {
            let (data, state) = mem_and_state(&mut c);
            let s = mem_slice(data, ptr as u32, len as u32)
                .and_then(|b| std::str::from_utf8(b).ok())
                .unwrap_or("")
                .to_string();
            state.add(HeapVal::Str(s)) as i32
        });

        Ok(())
    }

    /// 用真随机填充一个 heap buffer(Bytes 或 MemView)。
    fn fill_heap_buffer(c: &mut Caller<'_, HostState>, idx: u32) {
        let (data, state) = mem_and_state(c);
        match state.get(idx) {
            HeapVal::MemView { ptr, len } => {
                let (p, l) = (*ptr as usize, *len as usize);
                let _ = getrandom::getrandom(&mut data[p..p + l]);
            }
            HeapVal::Bytes(_) => {
                if let HeapVal::Bytes(b) = &mut state.heap[idx as usize] {
                    let _ = getrandom::getrandom(&mut b[..]);
                }
            }
            _ => {}
        }
    }

    fn heapval_to_string(v: &HeapVal) -> String {
        match v {
            HeapVal::Str(s) => s.clone(),
            HeapVal::Bool(b) => b.to_string(),
            _ => String::new(),
        }
    }
}

// ── WASM 句柄 + 上下文 ─────────────────────────────────────────────

/// 已实例化的 WASM 引擎句柄(可复用,构造多个 context)。
pub struct QoderAuth {
    inner: Mutex<WasmInner>,
}

struct WasmInner {
    store: Store<HostState>,
    instance: Instance,
    memory: Memory,
    malloc: TypedFunc<(i32, i32), i32>,
    add_sp: TypedFunc<i32, i32>,
}

impl QoderAuth {
    /// 加载内嵌 WASM 并实例化。
    pub fn new() -> Result<Self> {
        let engine = Engine::default();
        let module =
            Module::new(&engine, WASM_BYTES).map_err(|e| QoderAuthError::Engine(e.to_string()))?;
        let mut store = Store::new(&engine, HostState::new());
        let mut linker: Linker<HostState> = Linker::new(&engine);
        imports::register(&mut linker)?;
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| QoderAuthError::Engine(e.to_string()))?
            .start(&mut store)
            .map_err(|e| QoderAuthError::Engine(e.to_string()))?;

        // wasm-bindgen 的 init(若以导出函数形式提供)
        if let Ok(start) = instance.get_typed_func::<(), ()>(&store, "__wbindgen_start") {
            start
                .call(&mut store, ())
                .map_err(|e| QoderAuthError::Wasm(e.to_string()))?;
        }

        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| QoderAuthError::Engine("no memory export".into()))?;
        let malloc = instance
            .get_typed_func::<(i32, i32), i32>(&mut store, "__wbindgen_export2")
            .map_err(|e| QoderAuthError::Engine(e.to_string()))?;
        let add_sp = instance
            .get_typed_func::<i32, i32>(&mut store, "__wbindgen_add_to_stack_pointer")
            .map_err(|e| QoderAuthError::Engine(e.to_string()))?;

        Ok(QoderAuth {
            inner: Mutex::new(WasmInner {
                store,
                instance,
                memory,
                malloc,
                add_sp,
            }),
        })
    }

    /// httpdns 账号 id(静态内嵌;冒烟测用,验证 WASM 绑定正确)。
    pub fn httpdns_account_id(&self) -> Result<String> {
        let mut g = self.inner.lock().unwrap();
        g.call_string_ret("get_httpdns_account_id")
    }

    /// httpdns 签名 secret(静态内嵌)。
    pub fn httpdns_secret_key(&self) -> Result<String> {
        let mut g = self.inner.lock().unwrap();
        g.call_string_ret("get_httpdns_secret_key")
    }

    /// httpdns 完整 config JSON(静态内嵌)。
    pub fn httpdns_config(&self) -> Result<String> {
        let mut g = self.inner.lock().unwrap();
        g.call_string_ret("get_httpdns_config")
    }

    /// 完整签名链:构造 context → 喂 device token → 产出签名+加密的模型请求。
    ///
    /// 各 JSON 入参对齐官方 SDK(见 MOC-297):
    /// - `machine_id`:阶段一自生成的稳定设备 id
    /// - `cosy_version`:`"1.0.34"`
    /// - `user_info_json` / `user_info_for_auth_json`:`{uid, encrypt_user_info, key,
    ///   organization_id, organization_tags:[], data_policy_agreed, security_oauth_token}`
    ///   (`security_oauth_token` = device token;`organization_tags` 必须是数组)
    /// - `client_meta_json`:`{client_type:"6", business_product:"qoder_work",
    ///   business_type:"agent", scene:"assistant"}`
    /// - `endpoint`:`"https://gateway.qoder.com.cn"`
    /// - `body`:私有 `remoteChatAsk` JSON
    /// - `model_key` / `model_source`:如 `"q36fmodel"` / `"system"`
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_signed_request(
        &self,
        machine_id: &str,
        cosy_version: &str,
        user_info_json: &str,
        client_meta_json: &str,
        user_info_for_auth_json: &str,
        endpoint: &str,
        body: &str,
        model_key: &str,
        model_source: &str,
    ) -> Result<PreparedRequest> {
        let mut g = self.inner.lock().unwrap();
        let ctx = g.new_context(machine_id, cosy_version, user_info_json, client_meta_json)?;
        g.refresh_auth_fields(ctx, user_info_for_auth_json)?;
        let handle = g.prepare_infer_request(ctx, endpoint, body, model_key, model_source)?;
        let out = g.read_request_result(handle)?;
        g.free_context(ctx);
        Ok(out)
    }

    /// 解密 gateway 的加密响应(`Encode=1`):整段响应文本 → 解密后 JSON/SSE 文本。
    /// 对齐官方 `decryptServerResponse`(`RS().decrypt_server_response(await resp.text())`)。
    /// 非法/未加密输入会 [`QoderAuthError::Trap`](对齐官方 glue 的 try/catch 语义)。
    pub fn decrypt_server_response(&self, encrypted: &str) -> Result<String> {
        let mut g = self.inner.lock().unwrap();
        g.decrypt_server_response(encrypted)
    }
}

/// [`QoderAuth::prepare_signed_request`] 的产物 —— 可直接 `POST` 的签名请求。
#[derive(Debug, Clone)]
pub struct PreparedRequest {
    /// 完整 URL(含 query,如 `.../agent_chat_generation?FetchKeys=...&Encode=1`)。
    pub url: String,
    /// 全套 Cosy 签名头(`Authorization: Bearer COSY.<sig>`、`Cosy-Date`、`Cosy-User` …)。
    pub headers: Vec<(String, String)>,
    /// AES-GCM 加密后的请求体(不可读明文)。
    pub body: Vec<u8>,
}

impl WasmInner {
    /// 调一个 `() -> String`(retptr 模式)的导出函数。
    fn call_string_ret(&mut self, name: &str) -> Result<String> {
        let f = self
            .instance
            .get_typed_func::<i32, ()>(&mut self.store, name)
            .map_err(|e| QoderAuthError::Wasm(e.to_string()))?;
        let free = self
            .instance
            .get_typed_func::<(i32, i32, i32), ()>(&mut self.store, "__wbindgen_export4")
            .map_err(|e| QoderAuthError::Wasm(e.to_string()))?;

        let sp = self.add_sp.call(&mut self.store, -16).map_err(trap)?;
        f.call(&mut self.store, sp).map_err(trap)?;
        let data = self.memory.data(&self.store);
        let ptr = read_i32(data, sp as u32);
        let len = read_i32(data, (sp + 4) as u32);
        let bytes = mem_slice(data, ptr as u32, len as u32)
            .ok_or(QoderAuthError::Oob {
                ptr: ptr as u32,
                len: len as u32,
            })?
            .to_vec();
        let s = std::str::from_utf8(&bytes)?.to_string();
        self.add_sp.call(&mut self.store, 16).map_err(trap)?;
        free.call(&mut self.store, (ptr, len, 1)).map_err(trap)?;
        Ok(s)
    }

    /// `decrypt_server_response(text) -> text`(string→string,retptr 模式)。
    fn decrypt_server_response(&mut self, encrypted: &str) -> Result<String> {
        let f = self
            .instance
            .get_typed_func::<(i32, i32, i32), ()>(&mut self.store, "decrypt_server_response")
            .map_err(|e| QoderAuthError::Wasm(e.to_string()))?;
        let sp = self.add_sp.call(&mut self.store, -16).map_err(trap)?;
        let (ptr, len) = self.pass_str(encrypted)?;
        // 解密对非法输入会 throw(官方 glue try/catch)→ __wbindgen_throw 记 last_err
        // + 随后的 unreachable 让 wasmi trap;两种情况都归一成 Trap 错误。
        if let Err(e) = f.call(&mut self.store, (sp, ptr, len)) {
            self.add_sp.call(&mut self.store, 16).ok();
            let msg = self.store.data_mut().last_err.take();
            return Err(msg.map_or_else(|| trap(e), QoderAuthError::Trap));
        }
        let rptr = self.ret_i32(sp, 0);
        let rlen = self.ret_i32(sp, 4);
        self.add_sp.call(&mut self.store, 16).map_err(trap)?;
        if rptr == 0 {
            return Ok(String::new());
        }
        let data = self.memory.data(&self.store);
        let bytes = mem_slice(data, rptr as u32, rlen as u32)
            .ok_or(QoderAuthError::Oob {
                ptr: rptr as u32,
                len: rlen as u32,
            })?
            .to_vec();
        self.free_bytes(rptr, rlen)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// UTF-8 编码 `s` 写进 WASM 线性内存(malloc),返回 `(ptr, len)`(len=字节长)。
    /// 对齐官方 glue `passStringToWasm0` 的 `realloc===undefined` 简单路径。
    fn pass_str(&mut self, s: &str) -> Result<(i32, i32)> {
        let bytes = s.as_bytes();
        let len = bytes.len() as i32;
        let ptr = self.malloc.call(&mut self.store, (len, 1)).map_err(trap)?;
        let data = self.memory.data_mut(&mut self.store);
        write_bytes(data, ptr as u32, bytes);
        self.store.data_mut().vlen = len as u32;
        Ok((ptr, len))
    }

    /// 读 retptr+off 处的 i32(off 单位:字节)。
    fn ret_i32(&self, sp: i32, off: u32) -> i32 {
        read_i32(self.memory.data(&self.store), (sp + off as i32) as u32)
    }

    /// 取 heap 里的错误对象(`__wbg_Error`/throw 存的 Str)为消息。
    fn take_err(&mut self, idx: i32) -> QoderAuthError {
        if let Some(m) = self.store.data_mut().last_err.take() {
            return QoderAuthError::Trap(m);
        }
        match self.store.data_mut().take(idx as u32) {
            HeapVal::Str(s) => QoderAuthError::Trap(s),
            _ => QoderAuthError::Trap("<wasm error>".into()),
        }
    }

    /// `qodercontext_new(machineId, cosyVersion, userInfoJson, clientMetaJson)` → ctx ptr。
    fn new_context(&mut self, mi: &str, cv: &str, ui: &str, cm: &str) -> Result<i32> {
        let f = self
            .instance
            .get_typed_func::<(i32, i32, i32, i32, i32, i32, i32, i32, i32), ()>(
                &mut self.store,
                "qodercontext_new",
            )
            .map_err(|e| QoderAuthError::Wasm(e.to_string()))?;
        let sp = self.add_sp.call(&mut self.store, -16).map_err(trap)?;
        let (mp, ml) = self.pass_str(mi)?;
        let (cp, cl) = self.pass_str(cv)?;
        let (up, ul) = self.pass_str(ui)?;
        let (kp, kl) = self.pass_str(cm)?;
        f.call(&mut self.store, (sp, mp, ml, cp, cl, up, ul, kp, kl))
            .map_err(trap)?;
        let ptr = self.ret_i32(sp, 0);
        let errobj = self.ret_i32(sp, 4);
        let err = self.ret_i32(sp, 8);
        self.add_sp.call(&mut self.store, 16).map_err(trap)?;
        if err != 0 {
            return Err(self.take_err(errobj));
        }
        Ok(ptr)
    }

    /// `qodercontext_refreshAuthFields(ctx, userInfoForAuthJson)` —— 喂 device token。
    fn refresh_auth_fields(&mut self, ctx: i32, json: &str) -> Result<()> {
        let f = self
            .instance
            .get_typed_func::<(i32, i32, i32, i32), ()>(
                &mut self.store,
                "qodercontext_refreshAuthFields",
            )
            .map_err(|e| QoderAuthError::Wasm(e.to_string()))?;
        let sp = self.add_sp.call(&mut self.store, -16).map_err(trap)?;
        let (jp, jl) = self.pass_str(json)?;
        f.call(&mut self.store, (sp, ctx, jp, jl)).map_err(trap)?;
        let errobj = self.ret_i32(sp, 0);
        let err = self.ret_i32(sp, 4);
        self.add_sp.call(&mut self.store, 16).map_err(trap)?;
        if err != 0 {
            return Err(self.take_err(errobj));
        }
        Ok(())
    }

    /// `qodercontext_prepareInferRequest(ctx, endpoint, body, key, source)` → RequestResult handle。
    fn prepare_infer_request(
        &mut self,
        ctx: i32,
        endpoint: &str,
        body: &str,
        key: &str,
        source: &str,
    ) -> Result<i32> {
        let f = self
            .instance
            .get_typed_func::<(i32, i32, i32, i32, i32, i32, i32, i32, i32, i32), ()>(
                &mut self.store,
                "qodercontext_prepareInferRequest",
            )
            .map_err(|e| QoderAuthError::Wasm(e.to_string()))?;
        let sp = self.add_sp.call(&mut self.store, -16).map_err(trap)?;
        let (ep, el) = self.pass_str(endpoint)?;
        let (bp, bl) = self.pass_str(body)?;
        let (kp, kl) = self.pass_str(key)?;
        let (spp, spl) = self.pass_str(source)?;
        f.call(&mut self.store, (sp, ctx, ep, el, bp, bl, kp, kl, spp, spl))
            .map_err(trap)?;
        let handle = self.ret_i32(sp, 0);
        let errobj = self.ret_i32(sp, 4);
        let err = self.ret_i32(sp, 8);
        self.add_sp.call(&mut self.store, 16).map_err(trap)?;
        if err != 0 {
            return Err(self.take_err(errobj));
        }
        Ok(handle)
    }

    /// 读 RequestResult{url, headers(Map), body}。
    fn read_request_result(&mut self, handle: i32) -> Result<PreparedRequest> {
        let url = self.rr_string("requestresult_url", handle)?;
        // headers:requestresult_headers(handle) 直接返回 Map 的 heap idx
        let hf = self
            .instance
            .get_typed_func::<i32, i32>(&mut self.store, "requestresult_headers")
            .map_err(|e| QoderAuthError::Wasm(e.to_string()))?;
        let hidx = hf.call(&mut self.store, handle).map_err(trap)?;
        let headers = match self.store.data_mut().take(hidx as u32) {
            HeapVal::Map(m) => m.borrow().clone(),
            _ => Vec::new(),
        };
        let body = self.rr_bytes("requestresult_body", handle)?;
        Ok(PreparedRequest { url, headers, body })
    }

    /// 调 `requestresult_url/body`(retptr, handle) → String。
    fn rr_string(&mut self, name: &str, handle: i32) -> Result<String> {
        let (ptr, len) = self.rr_ptr_len(name, handle)?;
        if ptr == 0 {
            return Ok(String::new());
        }
        let data = self.memory.data(&self.store);
        let bytes = mem_slice(data, ptr as u32, len as u32)
            .ok_or(QoderAuthError::Oob {
                ptr: ptr as u32,
                len: len as u32,
            })?
            .to_vec();
        self.free_bytes(ptr, len)?;
        Ok(std::str::from_utf8(&bytes)?.to_string())
    }

    /// 同上但返回原始字节(加密 body,不做 UTF-8 解码)。
    fn rr_bytes(&mut self, name: &str, handle: i32) -> Result<Vec<u8>> {
        let (ptr, len) = self.rr_ptr_len(name, handle)?;
        if ptr == 0 {
            return Ok(Vec::new());
        }
        let data = self.memory.data(&self.store);
        let bytes = mem_slice(data, ptr as u32, len as u32)
            .ok_or(QoderAuthError::Oob {
                ptr: ptr as u32,
                len: len as u32,
            })?
            .to_vec();
        self.free_bytes(ptr, len)?;
        Ok(bytes)
    }

    fn rr_ptr_len(&mut self, name: &str, handle: i32) -> Result<(i32, i32)> {
        let f = self
            .instance
            .get_typed_func::<(i32, i32), ()>(&mut self.store, name)
            .map_err(|e| QoderAuthError::Wasm(e.to_string()))?;
        let sp = self.add_sp.call(&mut self.store, -16).map_err(trap)?;
        f.call(&mut self.store, (sp, handle)).map_err(trap)?;
        let ptr = self.ret_i32(sp, 0);
        let len = self.ret_i32(sp, 4);
        self.add_sp.call(&mut self.store, 16).map_err(trap)?;
        Ok((ptr, len))
    }

    fn free_bytes(&mut self, ptr: i32, len: i32) -> Result<()> {
        let free = self
            .instance
            .get_typed_func::<(i32, i32, i32), ()>(&mut self.store, "__wbindgen_export4")
            .map_err(|e| QoderAuthError::Wasm(e.to_string()))?;
        free.call(&mut self.store, (ptr, len, 1)).map_err(trap)
    }

    /// 释放 qodercontext(`__wbg_qodercontext_free(ptr, 0)`)。
    fn free_context(&mut self, ctx: i32) {
        if let Ok(f) = self
            .instance
            .get_typed_func::<(i32, i32), ()>(&mut self.store, "__wbg_qodercontext_free")
        {
            let _ = f.call(&mut self.store, (ctx, 0));
        }
    }
}

fn trap(e: wasmi::Error) -> QoderAuthError {
    QoderAuthError::Trap(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 冒烟测:验证 wasmtime 绑定能加载 WASM + 读回静态内嵌的 httpdns 常量。
    /// 期望值来自 Node POC 实测(MOC-297):account_id=183012。
    #[test]
    fn httpdns_statics_match_poc() {
        let qa = QoderAuth::new().expect("WASM 实例化");
        assert_eq!(qa.httpdns_account_id().unwrap(), "183012");
        let secret = qa.httpdns_secret_key().unwrap();
        assert_eq!(secret.len(), 32, "httpdns secret 应为 32 hex chars");
        let cfg = qa.httpdns_config().unwrap();
        assert!(cfg.contains("\"account_id\":\"183012\""), "config: {cfg}");
    }

    /// 完整签名链:喂合成凭证 → WASM 产出签名请求,断言 Cosy 头结构(对齐 Node POC /
    /// MOC-297 实测:Authorization=Bearer COSY.<sig>、Cosy-User=uid、加密 body)。
    #[test]
    fn prepare_signed_request_produces_cosy_headers() {
        let qa = QoderAuth::new().unwrap();
        let uid = "019f1c56-6fb1-712a-bcce-c9f15d8f62ae";
        let machine_id = "94f5ff34-a804-4c34-a794-316ebce406ea";
        let ui = format!(
            r#"{{"uid":"{uid}","encrypt_user_info":"","key":"","organization_id":"","organization_tags":[],"data_policy_agreed":true,"security_oauth_token":"dt-test-token"}}"#
        );
        let cm = r#"{"client_type":"6","business_product":"qoder_work","business_type":"agent","scene":"assistant"}"#;
        let body = r#"{"model":"q36fmodel","session_id":"t","messages":[{"role":"user","content":"hi"}],"model_config":{"key":"q36fmodel","source":"system"}}"#;
        let req = qa
            .prepare_signed_request(
                machine_id,
                "1.0.34",
                &ui,
                cm,
                &ui,
                "https://gateway.qoder.com.cn",
                body,
                "q36fmodel",
                "system",
            )
            .expect("prepare_signed_request");

        let h: std::collections::HashMap<&str, &str> = req
            .headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert!(
            req.url
                .contains("/algo/api/v2/service/pro/sse/agent_chat_generation"),
            "url={}",
            req.url
        );
        assert!(
            h.get("Authorization")
                .is_some_and(|a| a.starts_with("Bearer COSY.")),
            "authorization={:?}",
            h.get("Authorization")
        );
        assert_eq!(h.get("Cosy-User").copied(), Some(uid));
        assert_eq!(h.get("Cosy-MachineId").copied(), Some(machine_id));
        assert_eq!(h.get("Cosy-Version").copied(), Some("1.0.34"));
        assert_eq!(h.get("Cosy-ClientType").copied(), Some("6"));
        assert!(h.contains_key("Cosy-Date"), "missing Cosy-Date");
        assert!(!req.body.is_empty(), "加密 body 不应为空");
    }

    /// 冒烟:decrypt 绑定可调、非法输入不 panic(返回 Ok 或 Err 均可)。
    /// 真机加密响应的解密正确性需捕获样本,留 adapter 集成测。
    #[test]
    fn decrypt_server_response_does_not_panic() {
        let qa = QoderAuth::new().unwrap();
        let _ = qa.decrypt_server_response("not-a-valid-encrypted-payload");
    }
}
