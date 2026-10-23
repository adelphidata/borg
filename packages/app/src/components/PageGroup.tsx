import type { ReactNode } from "react";

/**
 * One top-level group of a long page: a rule above it, a light title, and an
 * optional action at the right. The
 * uppercase micro-labels inside stay the sub-headings, so the two levels
 * never read alike. The first group on the page drops the rule.
 */
export function PageGroup({
  title,
  action,
  children,
}: {
  title: string;
  action?: ReactNode;
  children: ReactNode;
}) {
  return (
    <section className="border-t border-line2 pt-8 first:border-t-0 first:pt-0">
      <div className="mb-5 flex min-h-8 items-center justify-between gap-3">
        <h2 className="m-0 min-w-0 text-[19px] leading-[1.25] font-light tracking-[-0.02em] text-ink">
          {title}
        </h2>
        {action}
      </div>
      {children}
    </section>
  );
}
