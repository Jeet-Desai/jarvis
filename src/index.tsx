/* @refresh reload */
import { render } from "solid-js/web";
import { Orb } from "./orb/Orb";
import { Hud } from "./hud/Hud";
// JetBrains Mono, bundled locally (no network fetch at runtime).
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "@fontsource/jetbrains-mono/700.css";
import "@fontsource/jetbrains-mono/800.css";
import "./global.css";

const root = document.getElementById("root") as HTMLElement;
const path = window.location.pathname;

if (path.startsWith("/hud")) {
  render(() => <Hud />, root);
} else {
  render(() => <Orb />, root);
}
