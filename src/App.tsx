import { useEffect, useState } from "react";
import { type AppInfo, commands, unwrap } from "./ipc";

export default function App() {
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    commands
      .appInfo()
      .then((result) => setInfo(unwrap(result)))
      .catch((e: unknown) => setError(String(e)));
  }, []);

  return (
    <>
      <div className="titlebar" data-tauri-drag-region />
      <main className="page">
        <h1>{info ? `Murmur v${info.version}` : "Murmur"}</h1>
        {info && <p>Data: {info.data_dir}</p>}
        {error && <p role="alert">{error}</p>}
      </main>
    </>
  );
}
