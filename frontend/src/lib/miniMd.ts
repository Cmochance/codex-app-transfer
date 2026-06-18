// 极简 markdown 渲染(#271,逐字移植旧 app.js renderMiniMd/applyInlineMd/truncateString)。
// 无外部依赖 + XSS 安全:先 escape HTML,再在已 escape 文本上跑 inline rule。
// 输出用于 v-html(内容已 sanitize,无 inline script,CSP 安全)。
// agents/memories/skills 预览 + conversations 详情共用。

export function escapeHtml(s: unknown): string {
  return String(s ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;')
}

export function applyInlineMd(text: string): string {
  let s = escapeHtml(text)
  // inline code `code` — 占位防止内部 ** _ 被吃
  const inlineCodes: string[] = []
  s = s.replace(/`([^`\n]+)`/g, (_, c) => {
    const idx = inlineCodes.push(c) - 1
    return `\x01IC${idx}\x01`
  })
  // links [text](url) — 仅 http(s)
  s = s.replace(/\[([^\]]+)\]\((https?:\/\/[^)\s]+)\)/g, (_, txt, url) => {
    const safeUrl = (url as string).replace(/"/g, '%22')
    return `<a href="${safeUrl}" target="_blank" rel="noreferrer noopener">${txt}</a>`
  })
  // bold **text**(non-greedy 防折叠)
  s = s.replace(/\*\*([^*\n]+?)\*\*/g, '<strong>$1</strong>')
  // italic *text* / _text_(避开 ** 残留的孤立 *,non-greedy;devin #272 fix)
  s = s.replace(/(^|[\s(])\*([^*\n]+?)\*(?=[\s).,!?:;]|$)/g, '$1<em>$2</em>')
  s = s.replace(/(^|[\s(])_([^_\n]+?)_(?=[\s).,!?:;]|$)/g, '$1<em>$2</em>')
  // restore inline code(inlineCodes 内容已是 escape 后形态,不可二次 escape)
  s = s.replace(/\x01IC(\d+)\x01/g, (_, i) => `<code>${inlineCodes[Number(i)]}</code>`)
  return s
}

export function renderMiniMd(input: string | null | undefined): string {
  if (!input) return ''
  const src = String(input).replace(/\r\n?/g, '\n')
  // 1. 抽走 fenced code block,placeholder 占位避免 inline rule 污染
  const codeBlocks: { lang: string; code: string }[] = []
  const body = src.replace(/```([a-zA-Z0-9_+-]*)\n([\s\S]*?)```/g, (_, lang, code) => {
    const idx = codeBlocks.push({ lang, code }) - 1
    return `\x00CODEBLOCK${idx}\x00`
  })
  // 2. 行级 + paragraph 渲染
  const lines = body.split('\n')
  const out: string[] = []
  let paragraphBuf: string[] = []
  let listBuf: string[] = []
  let listOrd = false
  const flushParagraph = () => {
    if (!paragraphBuf.length) return
    out.push(`<p>${applyInlineMd(paragraphBuf.join('\n'))}</p>`)
    paragraphBuf = []
  }
  const flushList = () => {
    if (!listBuf.length) return
    const tag = listOrd ? 'ol' : 'ul'
    out.push(`<${tag}>${listBuf.map((i) => `<li>${applyInlineMd(i)}</li>`).join('')}</${tag}>`)
    listBuf = []
    listOrd = false
  }
  for (const line of lines) {
    const phMatch = line.match(/^\x00CODEBLOCK(\d+)\x00$/)
    if (phMatch) {
      flushParagraph()
      flushList()
      const cb = codeBlocks[Number(phMatch[1])]
      out.push(`<pre class="codex-md-code"><code>${escapeHtml(cb.code)}</code></pre>`)
      continue
    }
    if (/^\s*$/.test(line)) {
      flushParagraph()
      flushList()
      continue
    }
    const head = line.match(/^(#{1,6})\s+(.*)$/)
    if (head) {
      flushParagraph()
      flushList()
      const level = head[1].length
      out.push(`<h${level}>${applyInlineMd(head[2])}</h${level}>`)
      continue
    }
    const ul = line.match(/^\s*[-*]\s+(.*)$/)
    const ol = line.match(/^\s*\d+\.\s+(.*)$/)
    if (ul || ol) {
      flushParagraph()
      const wantOrd = !!ol
      if (listOrd !== wantOrd && listBuf.length) flushList()
      listOrd = wantOrd
      listBuf.push((ul || ol)![1])
      continue
    }
    const bq = line.match(/^>\s?(.*)$/)
    if (bq) {
      flushParagraph()
      flushList()
      out.push(`<blockquote>${applyInlineMd(bq[1])}</blockquote>`)
      continue
    }
    if (/^\s*(---|\*\*\*|___)\s*$/.test(line)) {
      flushParagraph()
      flushList()
      out.push('<hr>')
      continue
    }
    flushList()
    paragraphBuf.push(line)
  }
  flushParagraph()
  flushList()
  return out.join('')
}

export function truncateString(s: string | null | undefined, n: number): string {
  if (!s || s.length <= n) return s || ''
  return `${s.slice(0, n)}\n… [前端预览截断,导出文件含完整内容]`
}
