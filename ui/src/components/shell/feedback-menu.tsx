"use client";

import {
  Menu,
  MenuContent,
  MenuGroup,
  MenuGroupLabel,
  MenuItem,
  MenuTrigger,
} from "@ui-sdk/react/menu";
import { ChevronDownIcon } from "@/components/icons";

const FEEDBACK_LINKS = [
  {
    label: "Ask a question or suggest an idea",
    href: "https://github.com/elm-os/crucible/discussions",
  },
  { label: "Report a bug", href: "https://github.com/elm-os/crucible/issues/new/choose" },
] as const;

// Plain links with no query string and no referrer, so GitHub learns nothing about this deployment.
export function FeedbackMenu() {
  return (
    <Menu>
      <MenuTrigger className="text-data">
        Feedback
        <ChevronDownIcon className="text-ink-soft" />
      </MenuTrigger>
      <MenuContent align="end">
        <MenuGroup>
          <MenuGroupLabel className="text-label">Opens GitHub in a new tab</MenuGroupLabel>
          {FEEDBACK_LINKS.map((link) => (
            <MenuItem
              key={link.href}
              className="text-data no-underline"
              render={(props) => (
                <a {...props} href={link.href} target="_blank" rel="noreferrer">
                  {link.label}
                </a>
              )}
            />
          ))}
        </MenuGroup>
      </MenuContent>
    </Menu>
  );
}
