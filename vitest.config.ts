import vue from "@vitejs/plugin-vue";
import { defineConfig } from "vitest/config";

// Frontend tests: App.vue in a simulated browser, with every Rust command faked (`npm test`).
export default defineConfig({
  plugins: [vue()],
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.ts"],
  },
});
