import { NavLink } from "react-router-dom";
import { routes, type AppRoute } from "../app/routes";
import { recents, user } from "../data/mock";
import { Brackets } from "./Brackets";
import { LogoIcon } from "./icons";

function NavItem({ route }: { route: AppRoute }) {
  const Icon = route.icon;
  return (
    // `end` only on the root route so parent items stay lit on detail pages (matches TabBar).
    <NavLink
      to={route.path}
      end={route.path === "/"}
      className={({ isActive }) =>
        `relative flex cursor-pointer items-center gap-[11px] rounded-[7px] px-[11px] py-[9px] text-[13.5px] leading-none font-medium tracking-[-0.01em] transition-[background,color] duration-150 ${
          isActive
            ? "bg-nav-on-bg text-nav-on-ink"
            : "bg-transparent text-ink2 hover:bg-surface3 hover:text-ink"
        }`
      }
    >
      {({ isActive }) => (
        <>
          <Icon size={16} />
          {route.label}
          {route.hasAlert && (
            <>
              <span className="flex-1" />
              <span className="h-1.5 w-1.5 flex-none rounded-full bg-alert" />
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
    <div className="flex flex-col gap-[3px]">
      {routes
        .filter((r) => r.section === section)
        .map((r) => (
          <NavItem key={r.id} route={r} />
        ))}
    </div>
  );
}

/** 1px divider between sidebar sections. */
function Rule({ className = "mb-4" }: { className?: string }) {
  return <div className={`mx-1.5 h-px bg-line2 ${className}`} />;
}

export function Sidebar() {
  return (
    <aside className="flex w-[236px] flex-none flex-col border-r border-line2 bg-surface2 px-[14px] pt-[18px] pb-4">
      <div className="flex items-center gap-[11px] px-2 pt-0.5 pb-5">
        <LogoIcon size={20} />
        <span className="text-[13px] leading-none font-semibold tracking-[0.2em] text-ink">
          BORG
        </span>
      </div>

      <Rule />
      <NavSection section="primary" />
      <Rule className="my-4" />
      <NavSection section="secondary" />
      <Rule className="mt-4 mb-3.5" />

      <div className="px-2 pb-2 text-[9.5px] leading-none font-medium tracking-[0.16em] uppercase text-ink3">
        Recent
      </div>
      <div className="flex flex-col gap-px">
        {recents.map((r) => (
          <div
            key={r}
            className="cursor-pointer overflow-hidden rounded-md px-3 py-[7px] text-[12.5px] leading-[1.35] text-ellipsis whitespace-nowrap text-ink3 hover:bg-surface3 hover:text-ink2"
          >
            {r}
          </div>
        ))}
      </div>

      <div style={{ flex: 1 }} />

      <div className="flex items-center gap-[11px] border-t border-line2 px-2 pt-3">
        <span className="flex h-[26px] w-[26px] items-center justify-center rounded-full border border-line bg-surface3 text-[10.5px] leading-none font-semibold text-ink2">
          {user.initials}
        </span>
        <div style={{ flex: 1, minWidth: 0 }}>
          <div className="text-[12.5px] leading-[1.2] font-medium text-ink">{user.name}</div>
        </div>
      </div>
    </aside>
  );
}
