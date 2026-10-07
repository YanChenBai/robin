<script setup lang="ts">
import ActionButton from "./ActionButton.vue";
defineProps<{
  label: string;
  loading?: boolean;
  disabled?: boolean;
  small?: boolean;
  primary?: boolean;
  icon?: string;
}>();
const emit = defineEmits<{ tap: [] }>();
</script>

<template>
  <GridLayout
    :height="small ? 36 : 48"
    :width="small ? 88 : undefined"
    marginTop="4"
    marginBottom="4"
  >
    <ActionButton
      :label="loading ? '' : label"
      :icon="loading ? undefined : (icon ?? 'connect')"
      :primary="primary"
      :small="small"
      margin="0"
      :disabled="disabled || loading"
      :accessibilityLabel="loading ? '正在连接电脑' : label"
      @tap="emit('tap')"
    />
    <StackLayout
      v-if="loading"
      orientation="horizontal"
      horizontalAlignment="center"
      verticalAlignment="center"
      :isUserInteractionEnabled="false"
    >
      <ActivityIndicator
        :busy="true"
        :width="small ? 14 : 18"
        :height="small ? 14 : 18"
        :color="primary ? '#100b18' : '#c4b5fd'"
        verticalAlignment="center"
      />
      <Label
        :text="small ? '连接中' : '正在连接…'"
        :fontSize="small ? 12 : 14"
        :color="primary ? '#100b18' : '#c4b5fd'"
        marginLeft="6"
        verticalAlignment="center"
      />
    </StackLayout>
  </GridLayout>
</template>
