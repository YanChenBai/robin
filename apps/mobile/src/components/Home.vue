<script setup lang="ts">
import ConnectionButton from "./ConnectionButton.vue";
import ActionButton from "./ActionButton.vue";
import { Application, ApplicationSettings, ImageSource, Utils } from "@nativescript/core";
import { computed, onMounted, onUnmounted, ref, shallowRef } from "nativescript-vue";
import {
  approve,
  connect,
  disconnect,
  reject,
  setAutoReconnect,
  setBuffer,
  setPaused,
  snapshot,
  startReceiver,
  stopReceiver,
  type ConnectionRecord,
  type ReceiverSnapshot,
} from "@robin/native";

// 新设置布局首次启用时采用新的默认值，之后保留用户主动修改。
if (ApplicationSettings.getNumber("settingsVersion", 0) < 1) {
  ApplicationSettings.setBoolean("autoStart", true);
  ApplicationSettings.setBoolean("autoBuffer", false);
  ApplicationSettings.setNumber("settingsVersion", 1);
}

const current = ref<ReceiverSnapshot>({ state: "stopped" });
const tab = ref<"player" | "settings">("player");
const remember = ref(true);
const bufferMs = ref(ApplicationSettings.getNumber("bufferMs", 20));
const automatic = ref(ApplicationSettings.getBoolean("autoBuffer", false));
const autoStart = ref(ApplicationSettings.getBoolean("autoStart", true));
const ip = ref(ApplicationSettings.getString("computerIp", ""));
const port = ref(ApplicationSettings.getString("computerPort", "4212"));
const formError = ref("");
let poll: ReturnType<typeof setInterval> | undefined;
let bufferUpdate: ReturnType<typeof setTimeout> | undefined;
const starting = ref(false);
const connectionRequest = ref<{
  address: string;
  fingerprint: string;
  name: string;
  startedAt: number;
  previousError: string;
}>();
const labels: Record<string, string> = {
  stopped: "接收已关闭",
  waiting: "准备就绪",
  connecting: "正在连接电脑",
  pending: "确认连接",
  buffering: "正在缓冲",
  playing: "正在播放",
  paused: "播放已暂停",
  error: "需要处理",
};
const status = computed(() =>
  connectionRequest.value
    ? "正在连接电脑"
    : starting.value
      ? "正在准备接收"
      : (labels[current.value.state] ?? current.value.state),
);
const history = computed(() => current.value.history ?? []);
const active = computed(
  () => starting.value || !["stopped", "error"].includes(current.value.state),
);
const connected = computed(() => ["buffering", "playing", "paused"].includes(current.value.state));
const busy = computed(
  () =>
    !!connectionRequest.value ||
    connected.value ||
    ["pending", "connecting"].includes(current.value.state),
);
const loading = computed(
  () =>
    !!connectionRequest.value ||
    starting.value ||
    ["connecting", "buffering"].includes(current.value.state),
);
const waveform = computed(() =>
  Array.from({ length: 32 }, (_, ix) => {
    const level = current.value.state === "playing" ? (current.value.waveform?.[ix] ?? 0) : 0;
    return Math.max(3, Math.sqrt(Math.min(1, level)) * 64);
  }),
);
const navigation = [
  { page: "player", label: "播放", icon: "navigation_play" },
  { page: "settings", label: "设置", icon: "navigation_settings" },
] as const;
const navigationIcons = shallowRef<Record<string, ImageSource>>({});
let initialized = false;

function initializePage() {
  if (initialized) return;
  initialized = true;
  // 页面加载后再读取原生资源、启动接收，避开首次渲染的初始化阶段。
  const context = Utils.android.getApplicationContext();
  const icons: Record<string, ImageSource> = {};
  for (const item of navigation) {
    const resource = context
      .getResources()
      .getIdentifier(item.icon, "drawable", context.getPackageName());
    const drawable = resource > 0 ? context.getDrawable(resource) : null;
    if (drawable) icons[item.page] = new ImageSource(drawable);
  }
  navigationIcons.value = icons;
  refresh();
  if (autoStart.value && current.value.state === "stopped") start();
  if (active.value && !starting.value) setBuffer(bufferMs.value, automatic.value);
  resumePolling();
}
const pageTitle = computed(() => (tab.value === "player" ? "播放" : "设置"));
const playerTitle = computed(() =>
  busy.value
    ? current.value.peerName || connectionRequest.value?.name || "正在连接电脑"
    : "连接后，即刻聆听",
);
const playerDescription = computed(() => {
  if (connected.value) return "48 kHz · 立体声 · PCM";
  if (current.value.state === "pending") return "核对下面的连接代码，允许电脑播放";
  if (loading.value)
    return connectionRequest.value
      ? `正在连接 ${connectionRequest.value.address}`
      : "正在连接并准备音频";
  if (!active.value) return "在设置中开启接收，连接电脑后开始播放";
  return "接收已开启，连接电脑即可播放";
});
const connectedComputer = computed(() =>
  history.value.find((item) => item.fingerprint === current.value.fingerprint),
);
const connectionMetrics = computed(() => {
  const state = current.value;
  const number = (value?: number) => (connected.value && value != null ? String(value) : "—");
  const milliseconds = (value?: number) =>
    connected.value && value != null ? `${value.toFixed(1)} ms` : "—";
  return [
    { label: "估算延迟", value: milliseconds(state.estimatedLatencyMs) },
    { label: "网络 RTT", value: milliseconds(state.networkRttMs) },
    { label: "接收缓冲", value: connected.value ? `${state.bufferMs ?? bufferMs.value} ms` : "—" },
    { label: "播放队列", value: milliseconds(state.queuedMs) },
    {
      label: "输出缓冲",
      value: milliseconds(state.outputFrames != null ? state.outputFrames / 48 : undefined),
    },
    { label: "已接收", value: number(state.received) },
    { label: "缺包", value: number(state.lost) },
    { label: "迟到", value: number(state.late) },
    { label: "播放欠载", value: number(state.underruns) },
    { label: "排序队列", value: milliseconds(state.jitterQueuedMs) },
  ];
});

function refresh() {
  try {
    const next = snapshot();
    if (next.state === "pending" && current.value.state !== "pending") tab.value = "player";
    const request = connectionRequest.value;
    if (request) {
      if (!["stopped", "waiting", "connecting"].includes(next.state))
        connectionRequest.value = undefined;
      else if (next.error && next.error !== request.previousError) {
        formError.value = next.error;
        connectionRequest.value = undefined;
      } else if (next.state !== "connecting" && Date.now() - request.startedAt > 10000) {
        formError.value = "连接未响应，请检查电脑地址与接收状态";
        connectionRequest.value = undefined;
      }
    }
    current.value = next;
  } catch (error) {
    connectionRequest.value = undefined;
    current.value = { ...current.value, state: "error", error: String(error) };
  }
  if (current.value.state !== "stopped") starting.value = false;
}
function start() {
  if (starting.value || active.value) return;
  try {
    starting.value = true;
    startReceiver(bufferMs.value, automatic.value);
  } catch (error) {
    starting.value = false;
    current.value = { ...current.value, state: "error", error: String(error) };
  }
}
function stop() {
  starting.value = false;
  connectionRequest.value = undefined;
  stopReceiver();
}
function configureBuffer(value: number, auto = automatic.value) {
  const next = Math.min(100, Math.max(5, Math.round(value)));
  if (next === bufferMs.value && auto === automatic.value) return;
  bufferMs.value = next;
  automatic.value = auto;
  ApplicationSettings.setNumber("bufferMs", next);
  ApplicationSettings.setBoolean("autoBuffer", auto);
  if (bufferUpdate) clearTimeout(bufferUpdate);
  bufferUpdate = setTimeout(() => {
    if (active.value) setBuffer(next, auto);
  }, 150);
}
function changeAutoStart(value: boolean) {
  autoStart.value = value;
  ApplicationSettings.setBoolean("autoStart", value);
}
function connectComputer(record?: ConnectionRecord) {
  if (busy.value || starting.value) return;
  formError.value = "";
  let address = record?.address;
  if (!record) {
    const host = ip.value.trim();
    const segments = host.split(".");
    const portNumber = Number(port.value);
    if (
      segments.length !== 4 ||
      segments.some((part) => !/^\d{1,3}$/.test(part) || Number(part) > 255) ||
      host === "0.0.0.0" ||
      !/^\d+$/.test(port.value) ||
      portNumber < 1 ||
      portNumber > 65535
    ) {
      formError.value = "请输入有效的电脑 IPv4 地址和端口（1–65535）";
      return;
    }
    address = `${host}:${portNumber}`;
    ApplicationSettings.setString("computerIp", host);
    ApplicationSettings.setString("computerPort", String(portNumber));
  }
  if (!address) {
    tab.value = "player";
    formError.value = "这条旧记录没有电脑地址，请手动连接一次";
    return;
  }
  connectionRequest.value = {
    address,
    fingerprint: record?.fingerprint ?? "",
    name: record?.name ?? address,
    startedAt: Date.now(),
    previousError: current.value.error ?? "",
  };
  try {
    connect(address, record?.fingerprint ?? "", bufferMs.value, automatic.value);
    tab.value = "player";
  } catch (error) {
    connectionRequest.value = undefined;
    tab.value = "player";
    formError.value = String(error);
  }
}
function allow() {
  approve(remember.value);
  refresh();
}
function decline() {
  reject();
  refresh();
}
function disconnectComputer() {
  disconnect();
  refresh();
}
function togglePlayback() {
  setPaused(!current.value.paused);
}
function changeReconnect(record: ConnectionRecord, enabled: boolean) {
  if (record.autoReconnect === enabled) return;
  try {
    setAutoReconnect(record.fingerprint, enabled);
    formError.value = "";
    refresh();
  } catch (error) {
    formError.value = `无法保存自动连接设置：${String(error)}`;
    refresh();
  }
}
function resumePolling() {
  refresh();
  if (!poll) poll = setInterval(refresh, 100);
}
function suspendPolling() {
  if (poll) clearInterval(poll);
  poll = undefined;
}
onMounted(() => {
  Application.on(Application.suspendEvent, suspendPolling);
  Application.on(Application.resumeEvent, resumePolling);
});
onUnmounted(() => {
  suspendPolling();
  Application.off(Application.suspendEvent, suspendPolling);
  Application.off(Application.resumeEvent, resumePolling);
  if (bufferUpdate) clearTimeout(bufferUpdate);
});
</script>

<template>
  <Frame>
    <Page class="robin-page" actionBarHidden="true" @loaded="initializePage">
      <GridLayout rows="auto, *, auto">
        <GridLayout row="0" columns="*, auto" class="header">
          <StackLayout col="0">
            <Label text="知更鸟 · Robin" class="eyebrow" />
            <Label :text="pageTitle" class="brand" />
          </StackLayout>
          <Label col="1" :text="status" class="header-status" verticalAlignment="center" />
        </GridLayout>
        <ScrollView :key="tab" row="1">
          <StackLayout class="content">
            <StackLayout v-if="current.state === 'pending'" class="confirmation">
              <Label text="允许电脑播放？" class="section-title" />
              <Label :text="current.peerName" class="computer-name" textWrap="true" />
              <Label :text="current.code" class="code" />
              <Label text="核对电脑上的连接代码一致后再允许" class="subtle" textWrap="true" />
              <GridLayout columns="*, auto" class="setting-row">
                <Label col="0" text="下次自动连接这台电脑" textWrap="true" />
                <Switch col="1" v-model="remember" accessibilityLabel="下次自动连接这台电脑" />
              </GridLayout>
              <GridLayout columns="*, *">
                <ActionButton col="0" label="拒绝" icon="close" @tap="decline" />
                <ActionButton col="1" label="允许播放" icon="check" primary @tap="allow" />
              </GridLayout>
            </StackLayout>
            <StackLayout v-if="tab === 'player'" class="player-page">
              <StackLayout class="player">
                <Label :text="busy ? '当前电脑' : '电脑音频'" class="player-caption" />
                <Label :text="playerTitle" class="player-title" textWrap="true" />
                <Label :text="playerDescription" class="subtle" textWrap="true" />
                <Label
                  v-if="connectedComputer?.address"
                  :text="connectedComputer.address"
                  class="hint"
                  textWrap="true"
                />
                <GridLayout
                  :columns="Array(32).fill('*').join(',')"
                  height="100"
                  class="waveform"
                  accessibilityLabel="实时音频波形"
                >
                  <StackLayout
                    v-for="(height, ix) in waveform"
                    :key="ix"
                    :col="ix"
                    :height="height"
                    class="wave-bar"
                    verticalAlignment="center"
                  />
                </GridLayout>
                <GridLayout v-if="connected" columns="*, auto" class="playback-actions">
                  <ActionButton
                    col="0"
                    :label="current.paused ? '继续播放' : '暂停播放'"
                    :icon="current.paused ? 'play' : 'pause'"
                    primary
                    @tap="togglePlayback"
                  />
                  <ActionButton col="1" label="断开" icon="disconnect" @tap="disconnectComputer" />
                </GridLayout>
                <Label v-if="current.state === 'pending'" :text="status" class="player-waiting" />
              </StackLayout>
              <StackLayout v-if="!connected" class="manual-form section-divider">
                <Label text="手动连接" class="setting-label" />
                <Label text="输入电脑上 Robin 显示的 IP 和端口" class="hint" textWrap="true" />
                <GridLayout columns="2*, *" class="address-form">
                  <StackLayout col="0" class="ip-field">
                    <Label text="电脑 IP" class="hint" />
                    <TextField
                      v-model="ip"
                      hint="192.168.1.8"
                      keyboardType="number"
                      :isEnabled="!busy"
                      accessibilityLabel="电脑 IP 地址"
                    />
                  </StackLayout>
                  <StackLayout col="1">
                    <Label text="端口" class="hint" />
                    <TextField
                      v-model="port"
                      hint="4212"
                      keyboardType="number"
                      :isEnabled="!busy"
                      accessibilityLabel="电脑接入端口"
                    />
                  </StackLayout>
                </GridLayout>
                <ConnectionButton
                  :label="current.state === 'pending' ? '等待确认' : '连接'"
                  :loading="loading"
                  :disabled="busy || starting"
                  primary
                  @tap="connectComputer()"
                />
              </StackLayout>
              <Label v-if="formError" :text="formError" class="error" textWrap="true" />
              <StackLayout class="saved-computers section-divider">
                <Label text="电脑记录" class="section-title" />
                <Label
                  v-if="!history.length"
                  text="在播放页手动连接电脑后，记录会自动保存"
                  class="subtle"
                  textWrap="true"
                />
                <StackLayout v-for="item in history" :key="item.fingerprint" class="computer-row">
                  <GridLayout columns="*, auto">
                    <StackLayout col="0" class="setting-copy">
                      <Label :text="item.name" class="computer-name" textWrap="true" />
                      <Label :text="item.address || '添加地址以重新连接'" class="hint" />
                      <Label
                        :text="
                          connected && current.fingerprint === item.fingerprint
                            ? '已连接'
                            : '未连接'
                        "
                        class="connection-state"
                      />
                    </StackLayout>
                    <ConnectionButton
                      col="1"
                      small
                      :icon="
                        connected && current.fingerprint === item.fingerprint
                          ? 'disconnect'
                          : 'connect'
                      "
                      :label="
                        connected && current.fingerprint === item.fingerprint ? '断开' : '连接'
                      "
                      :loading="
                        loading &&
                        (connectionRequest?.fingerprint === item.fingerprint ||
                          current.fingerprint === item.fingerprint)
                      "
                      :disabled="
                        starting ||
                        (busy && !(connected && current.fingerprint === item.fingerprint))
                      "
                      @tap="
                        connected && current.fingerprint === item.fingerprint
                          ? disconnectComputer()
                          : connectComputer(item)
                      "
                    />
                  </GridLayout>
                  <GridLayout columns="*, auto" class="record-preference">
                    <Label col="0" text="自动连接" class="subtle" />
                    <Switch
                      col="1"
                      :checked="item.autoReconnect"
                      :accessibilityLabel="`自动连接 ${item.name}`"
                      @checkedChange="changeReconnect(item, $event.value)"
                    />
                  </GridLayout>
                </StackLayout>
              </StackLayout>
              <StackLayout class="section-divider connection-details">
                <Label text="连接详情" class="section-title" />
                <GridLayout columns="*, *" rows="auto, auto, auto, auto, auto" class="metrics-grid">
                  <StackLayout
                    v-for="(item, ix) in connectionMetrics"
                    :key="item.label"
                    :row="Math.floor(ix / 2)"
                    :col="ix % 2"
                    class="metric-card"
                  >
                    <Label :text="item.label" class="hint" />
                    <Label :text="item.value" class="metric-value" />
                  </StackLayout>
                </GridLayout>
                <Label
                  :text="connected ? '延迟为估算值' : '连接电脑后显示实时数据'"
                  class="playback-note"
                  textWrap="true"
                />
              </StackLayout>
            </StackLayout>
            <StackLayout v-else>
              <StackLayout class="settings-group">
                <Label text="接收" class="section-title" />
                <GridLayout columns="*, auto" class="setting-row">
                  <StackLayout col="0" class="setting-copy">
                    <Label text="音频接收" class="setting-label" />
                    <Label
                      :text="active ? '已开启，可接收电脑音频' : '已关闭'"
                      class="hint"
                      textWrap="true"
                    />
                  </StackLayout>
                  <Switch
                    col="1"
                    :checked="active"
                    accessibilityLabel="开启音频接收"
                    @checkedChange="$event.value !== active && ($event.value ? start() : stop())"
                  />
                </GridLayout>
                <GridLayout columns="*, auto" class="setting-row">
                  <StackLayout col="0" class="setting-copy">
                    <Label text="启动时开启接收" class="setting-label" />
                    <Label text="打开应用就准备好接收声音" class="hint" textWrap="true" />
                  </StackLayout>
                  <Switch
                    col="1"
                    :checked="autoStart"
                    accessibilityLabel="启动时开启接收"
                    @checkedChange="changeAutoStart($event.value)"
                  />
                </GridLayout>
              </StackLayout>
              <StackLayout class="settings-group section-divider">
                <Label text="播放" class="section-title" />
                <GridLayout columns="*, auto" class="setting-row">
                  <StackLayout col="0" class="setting-copy">
                    <Label text="自适应缓冲" class="setting-label" />
                    <Label text="默认关闭。开启后根据网络情况调整" class="hint" textWrap="true" />
                  </StackLayout>
                  <Switch
                    col="1"
                    :checked="automatic"
                    accessibilityLabel="自适应缓冲"
                    @checkedChange="configureBuffer(bufferMs, $event.value)"
                  />
                </GridLayout>
                <GridLayout columns="*, auto" class="setting-row">
                  <StackLayout col="0" class="setting-copy">
                    <Label :text="automatic ? '最小接收缓冲' : '接收缓冲'" class="setting-label" />
                    <Label text="可在播放中调整，较大缓冲更稳定" class="hint" textWrap="true" />
                  </StackLayout>
                  <Label col="1" :text="`${bufferMs} ms`" class="buffer-value" />
                </GridLayout>
                <Slider
                  :value="bufferMs"
                  minValue="5"
                  maxValue="100"
                  accessibilityLabel="接收缓冲毫秒数"
                  @valueChange="configureBuffer($event.value)"
                />
                <GridLayout columns="*, *">
                  <Label col="0" text="低延迟 · 5 ms" class="hint" />
                  <Label col="1" text="更稳定 · 100 ms" class="hint" textAlignment="right" />
                </GridLayout>
                <Label
                  v-if="automatic && connected"
                  :text="`当前缓冲 ${current.bufferMs ?? bufferMs} ms`"
                  class="hint"
                />
                <Label text="通知栏可暂停或继续" class="hint" textWrap="true" />
              </StackLayout>
            </StackLayout>
            <Label
              v-if="tab === 'player' && current.error"
              :text="current.error"
              class="error"
              textWrap="true"
            />
          </StackLayout>
        </ScrollView>
        <GridLayout row="2" columns="*, *" class="navigation">
          <StackLayout
            v-for="(item, ix) in navigation"
            :key="item.page"
            :col="ix"
            :class="tab === item.page ? 'nav-item nav-selected' : 'nav-item'"
            :accessibilityLabel="`${item.label}${tab === item.page ? '，已选中' : ''}`"
            accessibilityRole="button"
            @tap="tab = item.page"
          >
            <Image
              :src="navigationIcons[item.page]"
              width="26"
              height="26"
              :tintColor="tab === item.page ? '#a78bfa' : '#96969f'"
              stretch="aspectFit"
            />
            <Label :text="item.label" class="nav-label" />
          </StackLayout>
        </GridLayout>
      </GridLayout>
    </Page>
  </Frame>
</template>
