import { initTheme } from "./services/theme";
import { bootstrapApp } from "./app/App";

initTheme();

const root = document.getElementById("app");
if (root) {
  void bootstrapApp(root);
}
