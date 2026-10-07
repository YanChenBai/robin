<script setup lang="ts">
import { Button, Utils, type EventData } from "@nativescript/core";
import { watch } from "nativescript-vue";

const props = defineProps<{
  label: string;
  icon?: string;
  disabled?: boolean;
  small?: boolean;
  primary?: boolean;
}>();
const emit = defineEmits<{ tap: [] }>();
let nativeButton: android.widget.Button | undefined;

function updateIcon() {
  if (!nativeButton) return;
  const context = Utils.android.getApplicationContext();
  const resource = props.icon
    ? context
        .getResources()
        .getIdentifier(`button_${props.icon}`, "drawable", context.getPackageName())
    : 0;
  const drawable = resource > 0 ? (context.getDrawable(resource)?.mutate() ?? null) : null;
  nativeButton.setCompoundDrawablesRelative(null, null, null, null);
  if (!drawable || !props.label) {
    nativeButton.setText(props.label);
    return;
  }
  const density = context.getResources().getDisplayMetrics().density;
  const size = Math.round((props.small ? 16 : 18) * density);
  drawable.setBounds(0, 0, size, size);
  drawable.setTint(android.graphics.Color.parseColor(props.primary ? "#100b18" : "#c4b5fd"));
  // 行内图标随文字整体居中，避免 compound drawable 被固定在按钮左边缘。
  const text = new android.text.SpannableString(`\uFFFC ${props.label}`);
  text.setSpan(
    new android.text.style.ImageSpan(drawable, android.text.style.ImageSpan.ALIGN_BOTTOM),
    0,
    1,
    android.text.Spanned.SPAN_EXCLUSIVE_EXCLUSIVE,
  );
  // Android 接受 CharSequence；NativeScript 声明将该参数统一映射为 string。
  nativeButton.setText(text as unknown as string);
}
function loaded(event: EventData) {
  nativeButton = (event.object as Button).nativeViewProtected;
  updateIcon();
}
function unloaded() {
  nativeButton = undefined;
}
watch(() => [props.label, props.icon, props.primary, props.small], updateIcon, { flush: "post" });
</script>

<template>
  <Button
    :text="label"
    :class="primary ? 'primary' : 'quiet'"
    :height="small ? 36 : 48"
    :minHeight="small ? 36 : 48"
    :fontSize="small ? 13 : 15"
    :isEnabled="!disabled"
    :accessibilityLabel="label"
    padding="0 12"
    @loaded="loaded"
    @unloaded="unloaded"
    @tap="emit('tap')"
  />
</template>
