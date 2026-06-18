import { ref } from "vue";

// 字体偏好:纯前端 UI 偏好,只存 localStorage(不经 /api/settings),启动即应用到 :root CSS 变量。
// 按「角色」分别设置(同一主题不同位置用不同字体):
//   正文 → --font-sans   标题 → --font-serif(米 h1/h2 用)   等宽 → --font-mono
// 默认值 = tokens.css 既有值(即「米主题」原本使用的字体):正文=系统 / 标题=宋体 / 等宽=mono。
// 字号 → 按比例缩放全部 --fs-* token。

export type FontChoice = "system" | "songti" | "kaiti" | "rounded" | "mono";
export type FontSize = "small" | "normal" | "large";

const STACK: Record<FontChoice, string> = {
  system:
    '-apple-system, BlinkMacSystemFont, "SF Pro Text", "SF Pro Display", "Segoe UI", "Microsoft YaHei UI", "PingFang SC", system-ui, sans-serif',
  songti: '"Songti SC", "STSong", "Source Han Serif SC", "SimSun", serif',
  kaiti: '"Kaiti SC", "STKaiti", "KaiTi", "Kaiti", serif',
  rounded:
    '"PingFang SC", "Hiragino Sans GB", "Microsoft YaHei", ui-rounded, "SF Pro Rounded", system-ui, sans-serif',
  mono: 'ui-monospace, "SF Mono", "Menlo", "Cascadia Code", "JetBrains Mono", monospace',
};

// 各角色:CSS 变量名 + 默认 choice(= 米原字体)+ localStorage key
const ROLES = {
  body: {
    varName: "--font-sans",
    def: "system" as FontChoice,
    key: "cas:fontBody",
  },
  heading: {
    varName: "--font-serif",
    def: "songti" as FontChoice,
    key: "cas:fontHeading",
  },
  mono: {
    varName: "--font-mono",
    def: "mono" as FontChoice,
    key: "cas:fontMono",
  },
} as const;
export type FontRole = keyof typeof ROLES;

const SIZE_KEY = "cas:fontSize";
const SIZE_SCALE: Record<FontSize, number> = {
  small: 0.9,
  normal: 1,
  large: 1.15,
};
// 与 tokens.css :root 默认值保持一致(缩放基准)
const FS_DEFAULTS: Record<string, number> = {
  "--fs-xs": 11,
  "--fs-sm": 12,
  "--fs-base": 13,
  "--fs-md": 14,
  "--fs-lg": 16,
  "--fs-xl": 20,
  "--fs-2xl": 26,
};

function readChoice(role: FontRole): FontChoice {
  const v = localStorage.getItem(ROLES[role].key) as FontChoice | null;
  return v && v in STACK ? v : ROLES[role].def;
}
function readSize(): FontSize {
  const v = localStorage.getItem(SIZE_KEY);
  return v === "small" || v === "large" ? v : "normal";
}

const body = ref<FontChoice>(readChoice("body"));
const heading = ref<FontChoice>(readChoice("heading"));
const mono = ref<FontChoice>(readChoice("mono"));
const size = ref<FontSize>(readSize());
const roleRefs: Record<FontRole, typeof body> = { body, heading, mono };

function applyRole(role: FontRole) {
  const root = document.documentElement;
  const { varName, def } = ROLES[role];
  const choice = roleRefs[role].value;
  // choice 等于角色默认时清除 override → 回落 tokens 默认值(米字体)
  if (choice === def) root.style.removeProperty(varName);
  else root.style.setProperty(varName, STACK[choice]);
}
function applySize() {
  const root = document.documentElement;
  const scale = SIZE_SCALE[size.value];
  for (const [k, base] of Object.entries(FS_DEFAULTS)) {
    if (scale === 1) root.style.removeProperty(k);
    else root.style.setProperty(k, `${Math.round(base * scale)}px`);
  }
}

// 模块加载即应用一次(main 引入后、首屏渲染前生效)
(["body", "heading", "mono"] as FontRole[]).forEach(applyRole);
applySize();

export function useFont() {
  function setRole(role: FontRole, choice: FontChoice) {
    roleRefs[role].value = choice;
    localStorage.setItem(ROLES[role].key, choice);
    applyRole(role);
  }
  function setSize(s: FontSize) {
    size.value = s;
    localStorage.setItem(SIZE_KEY, s);
    applySize();
  }
  return { body, heading, mono, size, setRole, setSize };
}
