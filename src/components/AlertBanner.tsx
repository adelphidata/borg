import type { Connection } from "../data/mock";
import { Pill } from "./Pill";

/** Red banner for a connection that needs the user. Copy comes from the data layer. */
export function AlertBanner({
  conn,
  stacked = false,
  onAction,
}: {
  conn: Connection;
  stacked?: boolean;
  onAction?: () => void;
}) {
  const alert = conn.alert;
  if (!alert) return null;

  if (stacked) {
    return (
      <div className="alert-banner stacked">
        <div className="head">
          <span className="dot" />
          <div className="title">{alert.stackedTitle}</div>
        </div>
        <div className="sub">{alert.sub}</div>
        <Pill variant="danger" style={{ alignSelf: "flex-start" }} onClick={onAction}>
          {alert.action}
        </Pill>
      </div>
    );
  }

  return (
    <div className="alert-banner">
      <span className="dot" />
      <div style={{ flex: 1, minWidth: 0 }}>
        <div className="title">{alert.title}</div>
        <div className="sub">{alert.sub}</div>
      </div>
      <Pill variant="danger" onClick={onAction}>
        {alert.action}
      </Pill>
    </div>
  );
}
