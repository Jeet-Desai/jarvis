/* @refresh reload */
import { render } from "solid-js/web";
import { Orb } from "./orb/Orb";
import { Hud } from "./hud/Hud";
import "./global.css";

const root = document.getElementById("root") as HTMLElement;
const path = window.location.pathname;

if (path.startsWith("/hud")) {
  render(() => <Hud />, root);
} else {
  render(() => <Orb />, root);
}
