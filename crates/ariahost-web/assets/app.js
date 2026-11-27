// AriaHost 网页管理页（vanilla ESM，零构建）。
//
// 只做四件事：看任务、加 URL、暂停/继续、移除。
// 所有请求都打同源的 /jsonrpc —— 服务端在那里注入 token，**本页永远拿不到 rpc-secret**。
// 安全起见：一律用 textContent 渲染用户数据，绝不拼 HTML 字符串
// （URL / 文件名里的引号和尖括号就是 XSS 入口）。

const POLL_MS = 2000;
const FIELDS = [
  "gid",
  "status",
  "totalLength",
  "completedLength",
  "downloadSpeed",
  "uploadSpeed",
  "files",
  "bittorrent",
  "errorMessage",
];

const rowsEl = document.getElementById("rows");
const emptyEl = document.getElementById("empty");
const statusEl = document.getElementById("status");
const noticeEl = document.getElementById("notice");
const formEl = document.getElementById("add");
const urlEl = document.getElementById("url");

/** 打一次同源 JSON-RPC；服务端的 503 会带一句人话，直接抛给调用方显示。 */
async function rpc(method, params = []) {
  const response = await fetch("/jsonrpc", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: Date.now() & 0xffff, method, params }),
  });

  let payload = null;
  try {
    payload = await response.json();
  } catch {
    payload = null;
  }

  if (!response.ok) {
    const detail = payload && payload.error && payload.error.message
      ? payload.error.message
      : "HTTP " + response.status;
    throw new Error(detail);
  }
  if (payload && payload.error) {
    throw new Error(payload.error.message || "下载服务返回了错误");
  }
  return payload ? payload.result : undefined;
}

function formatBytes(bytes) {
  const value = Number(bytes) || 0;
  if (value < 1024) return value + " B";
  const units = ["KB", "MB", "GB", "TB"];
  let scaled = value / 1024;
  let index = 0;
  while (scaled >= 1024 && index < units.length - 1) {
    scaled /= 1024;
    index += 1;
  }
  return scaled.toFixed(scaled >= 10 ? 0 : 1) + " " + units[index];
}

function formatSpeed(bytesPerSecond) {
  const value = Number(bytesPerSecond) || 0;
  return value > 0 ? formatBytes(value) + "/s" : "—";
}

function formatEta(task) {
  const total = Number(task.totalLength) || 0;
  const done = Number(task.completedLength) || 0;
  const speed = Number(task.downloadSpeed) || 0;
  if (task.status === "complete" || task.status === "removed") return "—";
  if (total <= 0 || speed <= 0) return "—";
  let seconds = Math.max(0, Math.floor((total - done) / speed));
  const hours = Math.floor(seconds / 3600);
  seconds -= hours * 3600;
  const minutes = Math.floor(seconds / 60);
  seconds -= minutes * 60;
  const two = (value) => String(value).padStart(2, "0");
  return hours > 0
    ? hours + ":" + two(minutes) + ":" + two(seconds)
    : two(minutes) + ":" + two(seconds);
}

function baseName(path) {
  if (!path) return "";
  const parts = String(path).split(/[\\/]/);
  return parts[parts.length - 1];
}

function taskName(task) {
  if (task.bittorrent && task.bittorrent.name) return task.bittorrent.name;
  const files = Array.isArray(task.files) ? task.files : [];
  if (files.length > 0 && files[0].path) return baseName(files[0].path);
  return task.gid || "任务";
}

function percentOf(task) {
  const total = Number(task.totalLength) || 0;
  const done = Number(task.completedLength) || 0;
  if (total <= 0) return task.status === "complete" ? 100 : 0;
  return Math.min(100, Math.round((done / total) * 100));
}

function statusText(task) {
  const map = {
    active: "下载中",
    waiting: "等待",
    paused: "已暂停",
    error: "出错",
    complete: "已完成",
    removed: "已移除",
  };
  return map[task.status] || task.status || "未知";
}

/** 一行任务。所有文本都走 textContent。 */
function buildRow(task) {
  const tr = document.createElement("tr");

  const nameCell = document.createElement("td");
  const name = document.createElement("div");
  name.className = "name";
  name.textContent = taskName(task);
  name.title = taskName(task);
  const meta = document.createElement("div");
  meta.className = "meta";
  // 状态做成「胶囊」：正绿点只给运行中，出错的走 --bad。
  // 深浅主题下都靠 CSS 令牌取色，脚本里不出现任何颜色字面量。
  const state = document.createElement("span");
  state.className =
    task.status === "active" ? "state active" : task.status === "error" ? "state error" : "state";
  state.textContent = statusText(task);
  meta.appendChild(state);
  if (task.errorMessage) {
    const detail = document.createElement("span");
    detail.textContent = " · " + task.errorMessage;
    meta.appendChild(detail);
  }
  nameCell.appendChild(name);
  nameCell.appendChild(meta);
  tr.appendChild(nameCell);

  const progressCell = document.createElement("td");
  const bar = document.createElement("div");
  bar.className = "bar";
  const fill = document.createElement("div");
  fill.className = "fill";
  fill.style.width = percentOf(task) + "%";
  bar.appendChild(fill);
  const percent = document.createElement("span");
  percent.className = "percent";
  percent.textContent = percentOf(task) + "%";
  progressCell.appendChild(bar);
  progressCell.appendChild(percent);
  tr.appendChild(progressCell);

  const speedCell = document.createElement("td");
  speedCell.className = "num";
  speedCell.textContent = formatSpeed(task.downloadSpeed);
  tr.appendChild(speedCell);

  const etaCell = document.createElement("td");
  etaCell.className = "num";
  etaCell.textContent = formatEta(task);
  tr.appendChild(etaCell);

  const actionsCell = document.createElement("td");
  actionsCell.className = "actions";
  const addButton = (label, method) => {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = label;
    button.addEventListener("click", () => act(method, task.gid));
    actionsCell.appendChild(button);
  };
  if (task.status === "active") addButton("暂停", "aria2.pause");
  if (task.status === "paused" || task.status === "waiting") addButton("继续", "aria2.unpause");
  if (task.status === "active" || task.status === "paused" || task.status === "waiting") {
    addButton("移除", "aria2.remove");
  }
  if (task.status === "complete" || task.status === "error" || task.status === "removed") {
    addButton("清除记录", "aria2.removeDownloadResult");
  }
  tr.appendChild(actionsCell);

  return tr;
}

function render(tasks) {
  rowsEl.textContent = "";
  emptyEl.hidden = tasks.length > 0;
  for (const task of tasks) rowsEl.appendChild(buildRow(task));
}

function setStatus(text, bad) {
  statusEl.textContent = text;
  statusEl.classList.toggle("bad", Boolean(bad));
}

function showNotice(text) {
  noticeEl.textContent = text;
  noticeEl.hidden = false;
}

function hideNotice() {
  noticeEl.hidden = true;
  noticeEl.textContent = "";
}

async function act(method, gid) {
  try {
    await rpc(method, [gid]);
    hideNotice();
    await tick();
  } catch (error) {
    showNotice(error.message);
  }
}

async function tick() {
  try {
    const [active, waiting, stopped, stat] = await Promise.all([
      rpc("aria2.tellActive", [FIELDS]),
      rpc("aria2.tellWaiting", [0, 100, FIELDS]),
      // tellStopped 的 offset 语义：0 = 从最旧端取；-k = 从倒数第 k 条向更旧取。
      // 想拿「最近 100 条」只有 -1 这一个写法；写成 -100 会取到「第 100 新 ~ 第 199 新」
      // 那一段，已停止数不足 100 时直接返回空（1.37.0 实测）。
      rpc("aria2.tellStopped", [-1, 100, FIELDS]),
      rpc("aria2.getGlobalStat", []),
    ]);
    hideNotice();
    setStatus(
      "↓ " + formatSpeed(stat.downloadSpeed) +
      "　↑ " + formatSpeed(stat.uploadSpeed) +
      "　活动 " + active.length + " / 等待 " + waiting.length,
      false
    );
    render([].concat(active, waiting, stopped));
  } catch (error) {
    // 最常见的就是 aria2 没起：服务端已经给了人话，这里原样显示，不抛堆栈。
    setStatus("未连接", true);
    showNotice(error.message);
  }
}

formEl.addEventListener("submit", async (event) => {
  event.preventDefault();
  const url = urlEl.value.trim();
  if (!url) return;
  try {
    await rpc("aria2.addUri", [[url]]);
    urlEl.value = "";
    hideNotice();
    await tick();
  } catch (error) {
    showNotice(error.message);
  }
});

tick();
setInterval(tick, POLL_MS);
