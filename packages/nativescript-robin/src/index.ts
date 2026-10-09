import { Utils, isAndroid } from "@nativescript/core";

export interface ConnectionRecord {
  fingerprint: string;
  name: string;
  address: string;
}
export interface ReceiverSnapshot {
  state: string;
  peerName?: string;
  fingerprint?: string;
  code?: string;
  error?: string;
  port?: number;
  bufferMs?: number;
  bufferFloorMs?: number;
  foregroundBufferMs?: number;
  backgroundBufferMs?: number;
  queuedMs?: number;
  jitterQueuedMs?: number;
  networkRttMs?: number;
  estimatedLatencyMs?: number;
  received?: number;
  lost?: number;
  late?: number;
  underruns?: number;
  outputFrames?: number;
  history?: ConnectionRecord[];
  waveform?: number[];
  autoBuffer?: boolean;
  autoConnect?: boolean;
  paused?: boolean;
}

declare const dev: {
  robin: {
    audio: {
      RobinAudio: {
        initialize(context: unknown): void;
        start(context: unknown, bufferMs: number, backgroundBufferMs: number, automatic: boolean): void;
        connect(
          context: unknown,
          address: string,
          fingerprint: string,
          bufferMs: number,
          backgroundBufferMs: number,
          automatic: boolean,
        ): void;
        pause(context: unknown, paused: boolean): void;
        stop(context: unknown): void;
        snapshot(): string;
        nativeApprove(): void;
        nativeReject(): void;
        nativeDisconnect(): void;
        nativeAutoConnect(enabled: boolean): void;
        nativeBuffer(bufferMs: number, backgroundBufferMs: number, automatic: boolean): void;
      };
    };
  };
};

function bridge() {
  if (!isAndroid) throw new Error("Robin currently supports Android only");
  dev.robin.audio.RobinAudio.initialize(Utils.android.getApplicationContext());
  return dev.robin.audio.RobinAudio;
}
export function startReceiver(bufferMs = 20, backgroundBufferMs = 20, automatic = false) {
  bridge().start(Utils.android.getApplicationContext(), bufferMs, backgroundBufferMs, automatic);
}
export function stopReceiver() {
  bridge().stop(Utils.android.getApplicationContext());
}
export function snapshot(): ReceiverSnapshot {
  return JSON.parse(bridge().snapshot()) as ReceiverSnapshot;
}
export function approve() {
  bridge().nativeApprove();
}
export function reject() {
  bridge().nativeReject();
}
export function disconnect() {
  bridge().nativeDisconnect();
}
export function setAutoConnect(enabled: boolean) {
  bridge().nativeAutoConnect(enabled);
}
export function connect(address: string, fingerprint = "", bufferMs = 20, backgroundBufferMs = 20, automatic = false) {
  bridge().connect(
    Utils.android.getApplicationContext(),
    address,
    fingerprint,
    bufferMs,
    backgroundBufferMs,
    automatic,
  );
}
export function setBuffer(bufferMs: number, backgroundBufferMs: number, automatic: boolean) {
  bridge().nativeBuffer(bufferMs, backgroundBufferMs, automatic);
}
export function setPaused(paused: boolean) {
  bridge().pause(Utils.android.getApplicationContext(), paused);
}
