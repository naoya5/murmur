import React from "react";
import ReactDOM from "react-dom/client";
import Pill from "./pill/Pill";
import "./styles.css";

async function boot(): Promise<void> {
  if (import.meta.env.DEV && !("__TAURI_INTERNALS__" in window)) {
    await import("./ipc/mock");
  }
  const root = document.getElementById("root");
  if (root) {
    ReactDOM.createRoot(root).render(
      <React.StrictMode>
        <Pill />
      </React.StrictMode>,
    );
  }
}

void boot();
