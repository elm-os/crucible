"use client";

import {
  Drawer,
  DrawerBackdrop,
  DrawerContent,
  DrawerTitle,
  DrawerTrigger,
} from "@ui-sdk/react/drawer";
import { useState } from "react";
import { MenuIcon } from "../icons";
import { Nav } from "./nav";

export function MobileNav() {
  const [open, setOpen] = useState(false);
  return (
    <Drawer side="left" open={open} onOpenChange={setOpen}>
      <DrawerTrigger aria-label="Open navigation" className="md:hidden">
        <MenuIcon />
      </DrawerTrigger>
      <DrawerBackdrop className="bg-midnight-ink/20" />
      <DrawerContent className="flex h-full w-72 max-w-[85vw] flex-col gap-4 border-e border-ash-border bg-parchment-white p-4">
        <DrawerTitle className="wordmark">Crucible</DrawerTitle>
        <Nav onNavigate={() => setOpen(false)} />
      </DrawerContent>
    </Drawer>
  );
}
