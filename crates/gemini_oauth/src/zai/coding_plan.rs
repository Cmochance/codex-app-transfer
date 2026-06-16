//! Coding Plan 换组织 API key(ZCode `Hm.resolveProviderApiKey` / `resolveBizApiKey`)。
//!
//! OAuth 拿到的是 ZCode 业务 JWT + provider access_token,**不能直接打模型**;
//! Coding Plan 档要再换出**组织 API key**(`<apiKey>.<secretKey>`)做模型面的
//! `Authorization: Bearer`。流程(ZCode `resolveBizApiKey`):
//!
//! 1. `GET <biz_base>/api/biz/customer/getCustomerInfo` → 取 org / project
//! 2. `GET <biz_base>/api/biz/v1/organization/<org>/projects/<proj>/api_keys`
//!    → 找 name=`zcode-api-key`,没有就 `POST` 建一个
//! 3. `GET <…>/api_keys/copy/<urlencode(apiKey)>` → `secretKey`
//! 4. 最终 key = `<apiKey>.<secretKey>`(z.ai 必须有 secretKey;bigmodel 可只用 apiKey)
//!
//! z.ai 多一步前置:oauth `data.zai.access_token` 要先经 [`fetch_business_token`]
//! (`POST <business_login_url>`)换成业务 access_token 再当 biz Bearer;bigmodel
//! 直接拿 `data.bigmodel.access_token` 当 biz Bearer。
//!
//! **信封容差**:ZCode `fetchRemoteData` 返回已解信封的 data(代码直接访问
//! `r.organizations` / `.find()` / `.secretKey`),但业务 login 读 `.data.access_token`
//! —— 两种取法并存。这里统一**防御式**解析:目标字段在 top-level 或 `data` 下都接。

use serde_json::Value;

use super::constants::{
    zcode_source_headers, ZaiProviderConfig, DEFAULT_ORG_NAME_HINT, DEFAULT_PROJECT_NAME_HINT,
    ZCODE_API_KEY_NAME,
};
use super::ZaiError;

/// z.ai 专属:oauth `data.zai.access_token` → 业务 access_token(`POST
/// business_login_url`,body `{token}`)。bigmodel 不走这步。
pub async fn fetch_business_token(
    http: &reqwest::Client,
    config: &ZaiProviderConfig,
    oauth_access_token: &str,
) -> Result<String, ZaiError> {
    let url = config.business_login_url.ok_or_else(|| {
        ZaiError::KeyResolution(format!(
            "provider {} 无 business_login_url,不该调 fetch_business_token",
            config.provider.wire_id()
        ))
    })?;
    let body = serde_json::json!({ "token": oauth_access_token });
    let mut req = http.post(url).json(&body);
    for (k, v) in zcode_source_headers() {
        req = req.header(k, v);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(ZaiError::Status {
            status: status.as_u16(),
            body: text,
        });
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| ZaiError::Parse(e.to_string()))?;
    check_business_code(&v)?;
    // 主路径 snake_case;ZCode 后端有 camelCase 变体(refresh_token 旁就带
    // `?? refreshToken` fallback),access_token 也兜一层 `accessToken`
    extract_str(&v, "access_token")
        .or_else(|| extract_str(&v, "accessToken"))
        .ok_or(ZaiError::MissingField("business token access_token"))
}

/// 换出组织 API key(`<apiKey>.<secretKey>`)。`bearer_token` 是裸 token(内部
/// 包成 `Authorization: Bearer <token>`)。
pub async fn resolve_org_api_key(
    http: &reqwest::Client,
    config: &ZaiProviderConfig,
    bearer_token: &str,
) -> Result<String, ZaiError> {
    let biz = config.biz_base;

    // 1. customer info → org / project
    let customer = biz_get(
        http,
        &format!("{biz}/api/biz/customer/getCustomerInfo"),
        bearer_token,
    )
    .await?;
    let (org, proj) = pick_org_and_project(&customer)?;

    // 2. api_keys 找 zcode-api-key,没有就建
    let keys_url = format!("{biz}/api/biz/v1/organization/{org}/projects/{proj}/api_keys");
    let list = biz_get(http, &keys_url, bearer_token).await?;
    let entry = match find_api_key_entry(&list, ZCODE_API_KEY_NAME) {
        Some(e) => e,
        None => {
            tracing::info!("zcode-api-key 不存在,创建新组织 key");
            biz_post(
                http,
                &keys_url,
                bearer_token,
                &serde_json::json!({ "name": ZCODE_API_KEY_NAME }),
            )
            .await?
        }
    };
    let api_key = extract_str(&entry, "apiKey")
        .filter(|s| !s.is_empty())
        .ok_or(ZaiError::MissingField("api_keys entry apiKey"))?;

    // 3. copy → secretKey
    let copy_url = format!(
        "{keys_url}/copy/{}",
        url::form_urlencoded::byte_serialize(api_key.as_bytes()).collect::<String>()
    );
    let copied = biz_get(http, &copy_url, bearer_token).await?;
    let secret_key = extract_str(&copied, "secretKey").filter(|s| !s.is_empty());

    // 4. 拼 key:有 secretKey 用 `<apiKey>.<secretKey>`(read_json 已查业务码,
    //    走到这里 copy 一定业务成功,`None` 干净代表「确实不带 secretKey」)
    let org_key = match secret_key {
        Some(secret) => format!("{api_key}.{secret}"),
        None if config.require_secret_key => {
            return Err(ZaiError::KeyResolution(
                "组织 key copy 未返回 secretKey,但该 provider 要求 secretKey".into(),
            ));
        }
        None => {
            // bigmodel 合法的 apiKey-only 形态;显式日志区分这条降级路径
            tracing::info!(
                provider = config.provider.wire_id(),
                "copy 未返 secretKey,按 apiKey-only 形态使用组织 key"
            );
            api_key
        }
    };
    // 纵深防御:最终 key 不该为空(api_key 已在抽取处过空,这里兜底防回归)
    if org_key.is_empty() {
        return Err(ZaiError::KeyResolution("换出的组织 key 为空".into()));
    }
    Ok(org_key)
}

/// GET 一个 biz 端点,返回解析后的 JSON。
async fn biz_get(http: &reqwest::Client, url: &str, bearer: &str) -> Result<Value, ZaiError> {
    let mut req = http
        .get(url)
        .bearer_auth(bearer)
        .header("Content-Type", "application/json");
    for (k, v) in zcode_source_headers() {
        req = req.header(k, v);
    }
    read_json(req).await
}

/// POST 一个 biz 端点(JSON body),返回解析后的 JSON。
async fn biz_post(
    http: &reqwest::Client,
    url: &str,
    bearer: &str,
    body: &Value,
) -> Result<Value, ZaiError> {
    let mut req = http
        .post(url)
        .bearer_auth(bearer)
        .header("Content-Type", "application/json")
        .json(body);
    for (k, v) in zcode_source_headers() {
        req = req.header(k, v);
    }
    read_json(req).await
}

async fn read_json(req: reqwest::RequestBuilder) -> Result<Value, ZaiError> {
    let resp = req.send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(ZaiError::Status {
            status: status.as_u16(),
            body: text,
        });
    }
    let v: Value = serde_json::from_str(&text).map_err(|e| ZaiError::Parse(e.to_string()))?;
    // **关键**:ZCode 后端常以「HTTP 200 + body `{code:<err>,msg}`」表达业务错
    // (鉴权过期 / 限流 / 无权限)。必须查业务码,否则 200+业务错被当成功往下传,
    // 真因 msg 丢失、最终以误导性的 MissingField/KeyResolution 冒出(silent-failure
    // CRITICAL-1 修)。code 缺省(已解信封 / 扁平响应)时跳过 = 成功。
    check_business_code(&v)?;
    Ok(v)
}

/// ZCode `{code,msg,data}` 业务码白名单检查(`isSuccessfulRemoteCode`):code 缺省 /
/// null / 0 / 200 算成功,其余抛 [`ZaiError::Business`] 携带上游真实 msg。
fn check_business_code(v: &Value) -> Result<(), ZaiError> {
    if let Some(code) = v.get("code") {
        if !is_success_code(code) {
            return Err(ZaiError::Business {
                code: code.as_i64().unwrap_or(-1),
                msg: v
                    .get("msg")
                    .and_then(|m| m.as_str())
                    .unwrap_or_default()
                    .to_string(),
            });
        }
    }
    Ok(())
}

/// 从 customer info 选 org + project(ZCode `pickOrgAndProject`/`CN`):
/// 优先名字含「默认机构」/「默认项目」,匹配不到回退第一个。
///
/// 失败分两类(silent-failure MEDIUM-1):**空**(账号真无 org/project,可引导
/// 用户去开)走 [`ZaiError::KeyResolution`];**字段缺失/类型不符**(ZCode wire 改了
/// 字段名等结构漂移)走 [`ZaiError::MissingField`] —— 不再坍缩成同一个错,排查时
/// 能区分「账号状态」与「我们解析口径过时」。
pub(crate) fn pick_org_and_project(customer: &Value) -> Result<(String, String), ZaiError> {
    let orgs = field(customer, "organizations")
        .and_then(|v| v.as_array())
        .ok_or(ZaiError::MissingField("getCustomerInfo.organizations"))?;
    if orgs.is_empty() {
        return Err(ZaiError::KeyResolution(
            "账号下没有任何组织(organizations 为空),无法换组织 key".into(),
        ));
    }
    let org = orgs
        .iter()
        .find(|o| {
            o.get("organizationName")
                .and_then(|v| v.as_str())
                .is_some_and(|n| n.contains(DEFAULT_ORG_NAME_HINT))
        })
        .unwrap_or(&orgs[0]);
    let org_id = org
        .get("organizationId")
        .and_then(id_string)
        .ok_or(ZaiError::MissingField("organizationId"))?;

    let projects = org
        .get("projects")
        .and_then(|v| v.as_array())
        .ok_or(ZaiError::MissingField("organization.projects"))?;
    if projects.is_empty() {
        return Err(ZaiError::KeyResolution(
            "组织下没有任何项目(projects 为空),无法换组织 key".into(),
        ));
    }
    let proj = projects
        .iter()
        .find(|p| {
            p.get("projectName")
                .and_then(|v| v.as_str())
                .is_some_and(|n| n.contains(DEFAULT_PROJECT_NAME_HINT))
        })
        .unwrap_or(&projects[0]);
    let proj_id = proj
        .get("projectId")
        .and_then(id_string)
        .ok_or(ZaiError::MissingField("projectId"))?;
    Ok((org_id, proj_id))
}

/// 在 api_keys 列表里找 name 匹配的项(列表可能是顶层数组或 `data` 下数组)。
fn find_api_key_entry(list: &Value, name: &str) -> Option<Value> {
    let arr = list
        .as_array()
        .or_else(|| list.get("data").and_then(|d| d.as_array()))?;
    arr.iter()
        .find(|e| e.get("name").and_then(|v| v.as_str()) == Some(name))
        .cloned()
}

/// 防御式取字段:先 top-level,再 `data` 下(应对 ZCode 两种解信封口径)。
/// 返回字符串值(`apiKey`/`secretKey`/`access_token` 等)。
fn extract_str(value: &Value, key: &str) -> Option<String> {
    field(value, key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
}

/// 防御式取字段(返回 `&Value`):top-level 优先,否则 `data` 下。
fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    if let Some(v) = value.get(key) {
        return Some(v);
    }
    value.get("data").and_then(|d| d.get(key))
}

/// org/project id 可能是字符串或数字,统一成字符串(ZCode 直接拼进 URL 模板)。
fn id_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// ZCode `isSuccessfulRemoteCode`:null→true,数字 0|200→true,字符串 "0"|"200"→true。
fn is_success_code(code: &Value) -> bool {
    match code {
        Value::Null => true,
        Value::Number(n) => n.as_i64().map(|i| i == 0 || i == 200).unwrap_or(false),
        Value::String(s) => s == "0" || s == "200",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pick_org_project_prefers_default_named() {
        let customer = json!({
            "organizations": [
                {"organizationId": "org-A", "organizationName": "其它机构", "projects": [
                    {"projectId":"p-x","projectName":"杂项"}
                ]},
                {"organizationId": "org-B", "organizationName": "我的默认机构", "projects": [
                    {"projectId":"p-1","projectName":"随便"},
                    {"projectId":"p-2","projectName":"默认项目空间"}
                ]}
            ]
        });
        let (org, proj) = pick_org_and_project(&customer).unwrap();
        assert_eq!(org, "org-B", "应选名字含『默认机构』的");
        assert_eq!(proj, "p-2", "应选名字含『默认项目』的");
    }

    #[test]
    fn pick_org_project_falls_back_to_first() {
        let customer = json!({
            "organizations": [
                {"organizationId": 12345, "organizationName": "Acme", "projects": [
                    {"projectId": 678, "projectName": "Main"}
                ]}
            ]
        });
        // 数字 id 也能转字符串,无『默认』命名时回退第一个
        let (org, proj) = pick_org_and_project(&customer).unwrap();
        assert_eq!(org, "12345");
        assert_eq!(proj, "678");
    }

    #[test]
    fn pick_org_project_handles_data_wrapped_envelope() {
        let customer = json!({"data": {"organizations": [
            {"organizationId":"o","organizationName":"x","projects":[{"projectId":"p","projectName":"y"}]}
        ]}});
        let (org, proj) = pick_org_and_project(&customer).unwrap();
        assert_eq!((org.as_str(), proj.as_str()), ("o", "p"));
    }

    #[test]
    fn pick_org_project_distinguishes_empty_from_malformed() {
        // 空 organizations → KeyResolution(账号真无 org,可引导用户)
        let empty = pick_org_and_project(&json!({"organizations": []})).unwrap_err();
        assert!(
            matches!(empty, ZaiError::KeyResolution(_)),
            "实际 {empty:?}"
        );
        // organizations 字段缺失 → MissingField(结构漂移,跟「真无 org」区分)
        let missing = pick_org_and_project(&json!({"foo": 1})).unwrap_err();
        assert!(
            matches!(missing, ZaiError::MissingField(_)),
            "实际 {missing:?}"
        );
        // 有 org 但 projects 为空 → KeyResolution
        let no_proj =
            pick_org_and_project(&json!({"organizations":[{"organizationId":"o","projects":[]}]}))
                .unwrap_err();
        assert!(
            matches!(no_proj, ZaiError::KeyResolution(_)),
            "实际 {no_proj:?}"
        );
    }

    #[test]
    fn find_api_key_entry_matches_name_in_array_or_data() {
        let flat = json!([{"name":"other","apiKey":"a1"},{"name":"zcode-api-key","apiKey":"ak-2"}]);
        let e = find_api_key_entry(&flat, "zcode-api-key").unwrap();
        assert_eq!(e.get("apiKey").unwrap(), "ak-2");

        let wrapped = json!({"data":[{"name":"zcode-api-key","apiKey":"ak-w"}]});
        let e2 = find_api_key_entry(&wrapped, "zcode-api-key").unwrap();
        assert_eq!(e2.get("apiKey").unwrap(), "ak-w");

        assert!(find_api_key_entry(&json!([{"name":"nope"}]), "zcode-api-key").is_none());
    }

    #[test]
    fn extract_str_trims_and_unwraps_data() {
        assert_eq!(
            extract_str(&json!({"secretKey":" sk-9 "}), "secretKey").as_deref(),
            Some("sk-9")
        );
        assert_eq!(
            extract_str(&json!({"data":{"access_token":"at-1"}}), "access_token").as_deref(),
            Some("at-1")
        );
        assert_eq!(extract_str(&json!({}), "missing"), None);
    }

    #[test]
    fn check_business_code_rejects_200_with_error_envelope() {
        // CRITICAL-1:HTTP 200 但 body code!=0 必须报 Business(携真实 msg),不当成功
        let err = check_business_code(&json!({"code": 40001, "msg": "token invalid"})).unwrap_err();
        match err {
            ZaiError::Business { code, msg } => {
                assert_eq!(code, 40001);
                assert_eq!(msg, "token invalid");
            }
            o => panic!("应为 Business 错: {o:?}"),
        }
        // 缺省 code(已解信封 / 扁平响应)或 code=0 = 成功,不误伤
        assert!(check_business_code(&json!({"organizations": []})).is_ok());
        assert!(check_business_code(&json!({"code": 0, "data": {}})).is_ok());
        assert!(check_business_code(&json!({"code": 200})).is_ok());
    }

    #[test]
    fn is_success_code_matches_zcode_whitelist() {
        assert!(is_success_code(&json!(null)));
        assert!(is_success_code(&json!(0)));
        assert!(is_success_code(&json!(200)));
        assert!(is_success_code(&json!("0")));
        assert!(is_success_code(&json!("200")));
        assert!(!is_success_code(&json!(40001)));
        assert!(!is_success_code(&json!("fail")));
    }
}
