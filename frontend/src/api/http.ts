// 移植旧 frontend/js/api.js 的 api(method,path,body) 核心(逐字保留语义):
// - header X-CAS-Request:1
// - 非 JSON 响应兜底(网关 502/504 / 长阻塞中断 → HTML/空 body)抛带 status 的清晰错误
// - !resp.ok || data.success===false 抛带结构化 errors[] + responseData 的 Error

export interface ApiError extends Error {
  errors: unknown[]
  responseData: unknown
}

const BASE = ''

export async function api<T = unknown>(method: string, path: string, body?: unknown): Promise<T> {
  const headers: Record<string, string> = { 'X-CAS-Request': '1' }
  const opts: RequestInit = { method, headers }
  if (body !== undefined) {
    headers['Content-Type'] = 'application/json'
    opts.body = JSON.stringify(body)
  }
  const resp = await fetch(BASE + path, opts)

  let data: { success?: boolean; message?: string; errors?: unknown[] } & Record<string, unknown>
  try {
    data = await resp.json()
  } catch (parseErr) {
    const error = new Error(
      `Request failed: ${method} ${path} — HTTP ${resp.status} ${resp.statusText || ''} ` +
        `(非 JSON 响应, 可能是网关错误或服务未就绪)`,
    ) as ApiError
    error.errors = []
    error.responseData = { status: resp.status, parseError: String(parseErr) }
    throw error
  }

  if (!resp.ok || data.success === false) {
    const error = new Error(data.message || `Request failed: ${method} ${path}`) as ApiError
    error.errors = Array.isArray(data.errors) ? data.errors : []
    error.responseData = data
    throw error
  }
  return data as T
}
