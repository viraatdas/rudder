import { render } from "preact";
import { BoardView, ProjectIndex } from "./components";
import { UsageView } from "./usage";

declare global {
  interface Window {
    __RUDDER_SLUG__?: string;
    __RUDDER_TOKEN__?: string;
    __RUDDER_CONTROL_MODE__?: "projector" | "scheduler";
    __RUDDER_CAN_MUTATE__?: boolean;
    __RUDDER_VIEW__?: "board" | "usage";
  }
}

function App() {
  const slug = (window.__RUDDER_SLUG__ ?? "").trim();
  if (window.__RUDDER_VIEW__ === "usage") return <UsageView />;
  return slug ? <BoardView slug={slug} /> : <ProjectIndex />;
}

const mount = document.getElementById("app");
if (mount) {
  render(<App />, mount);
}
