import { Utils, isAndroid } from "@nativescript/core";

export interface ConnectionRecord {
  fingerprint: string;
  name: string;
  autoReconnect: boolean;
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
  paused?: boolean;
}

declare const dev: {
  robin: {
    audio: {
      RobinAudio: {
        initialize(context: unknown): void;
        start(context: unknown, bufferMs: number, automatic: boolean): void;
        connect(
          context: unknown,
          address: string,
          fingerprint: string,
          bufferMs: number,
          automatic: boolean,
        ): void;
        pause(context: unknown, paused: boolean): void;
        stop(context: unknown): void;
        snapshot(): string;
        nativeApprove(remember: boolean): void;
        nativeReject(): void;
        nativeDisconnect(): void;
        nativeAutoReconnect(fingerprint: string, enabled: boolean): void;
        nativeBuffer(bufferMs: number, automatic: boolean): void;
      };
    };
  };
};

function bridge() {
  if (!isAndroid) throw new Error("Robin currently supports Android only");
  dev.robin.audio.RobinAudio.initialize(Utils.android.getApplicationContext());
  return dev.robin.audio.RobinAudio;
}
export function startReceiver(bufferMs = 20, automatic = false) {
  bridge().start(Utils.android.getApplicationContext(), bufferMs, automatic);
}
export function stopReceiver() {
  bridge().stop(Utils.android.getApplicationContext());
}
export function snapshot(): ReceiverSnapshot {
  return JSON.parse(bridge().snapshot()) as ReceiverSnapshot;
}
export function approve(remember: boolean) {
  bridge().nativeApprove(remember);
}
export function reject() {
  bridge().nativeReject();
}
export function disconnect() {
  bridge().nativeDisconnect();
}
export function setAutoReconnect(fingerprint: string, enabled: boolean) {
  bridge().nativeAutoReconnect(fingerprint, enabled);
}
export function connect(address: string, fingerprint = "", bufferMs = 20, automatic = false) {
  bridge().connect(
    Utils.android.getApplicationContext(),
    address,
    fingerprint,
    bufferMs,
    automatic,
  );
}
export function setBuffer(bufferMs: number, automatic: boolean) {
  bridge().nativeBuffer(bufferMs, automatic);
}
export function setPaused(paused: boolean) {
  bridge().pause(Utils.android.getApplicationContext(), paused);
}
