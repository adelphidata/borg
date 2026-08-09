import { NavLink } from "react-router-dom";
import { routes } from "../app/routes";

/** Bottom tab bar shown on phone-sized windows. */
export function TabBar() {
  return (
    <nav className="flex h-[72px] flex-none items-start justify-between border-t border-line2 bg-surface px-[26px] pt-[13px] pb-[env(safe-area-inset-bottom)]">
      {routes.map((r) => (
        <NavLink
          key={r.id}
          to={r.path}
          end={r.path === "/"}
          className="flex min-w-12 cursor-pointer flex-col items-center gap-[7px]"
        >
          {({ isActive }) => (
            <>
              <span
                className={`h-1.5 w-1.5 rounded-full ${
                  isActive ? "bg-neon shadow-(--glow)" : "bg-transparent"
                }`}
              />
              <span
                className={`text-[9.5px] leading-none font-medium tracking-[0.1em] uppercase ${
                  isActive ? "text-ink" : "text-ink3"
                }`}
              >
                {r.tabLabel}
              </span>
            </>
          )}
        </NavLink>
      ))}
    </nav>
  );
}
