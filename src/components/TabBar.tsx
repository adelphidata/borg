import { NavLink } from "react-router-dom";
import { routes } from "../app/routes";

/** Bottom tab bar shown on phone-sized windows. */
export function TabBar() {
  return (
    <nav className="tab-bar">
      {routes.map((r) => (
        <NavLink
          key={r.id}
          to={r.path}
          end={r.path === "/"}
          className={({ isActive }) => `tab-item${isActive ? " on" : ""}`}
        >
          <span className="dot" />
          <span className="label">{r.tabLabel}</span>
        </NavLink>
      ))}
    </nav>
  );
}
