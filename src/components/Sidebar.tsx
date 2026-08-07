import { NavLink } from "react-router-dom";
import { routes, type AppRoute } from "../app/routes";
import { recents, user } from "../data/mock";
import { Brackets } from "./Brackets";
import { LogoIcon } from "./icons";

function NavItem({ route }: { route: AppRoute }) {
  const Icon = route.icon;
  return (
    <NavLink to={route.path} end className={({ isActive }) => `nav-item${isActive ? " on" : ""}`}>
      {({ isActive }) => (
        <>
          <Icon size={16} />
          {route.label}
          {route.hasAlert && (
            <>
              <span className="spacer" />
              <span className="alert-dot" />
            </>
          )}
          {isActive && <Brackets />}
        </>
      )}
    </NavLink>
  );
}

function NavSection({ section }: { section: AppRoute["section"] }) {
  return (
    <div className="nav-group">
      {routes
        .filter((r) => r.section === section)
        .map((r) => (
          <NavItem key={r.id} route={r} />
        ))}
    </div>
  );
}

export function Sidebar() {
  return (
    <aside className="sidebar">
      <div className="sidebar-brand">
        <LogoIcon size={20} />
        <span>BORG</span>
      </div>

      <div className="sidebar-rule" />
      <NavSection section="primary" />
      <div className="sidebar-rule mid" />
      <NavSection section="secondary" />
      <div className="sidebar-rule low" />

      <div className="recent-label">Recent</div>
      <div className="recent-list">
        {recents.map((r) => (
          <div key={r} className="recent-item">
            {r}
          </div>
        ))}
      </div>

      <div style={{ flex: 1 }} />

      <div className="sidebar-user">
        <span className="avatar">{user.initials}</span>
        <div style={{ flex: 1, minWidth: 0 }}>
          <div className="name">{user.name}</div>
        </div>
      </div>
    </aside>
  );
}
