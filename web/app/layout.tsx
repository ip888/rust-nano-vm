import type { Metadata } from "next";
import type { ReactNode } from "react";

import "./globals.css";

export const metadata: Metadata = {
  title: "nanovm — sub-second microVMs for AI agents",
  description:
    "Fork a real KVM microVM in ~12 ms. Give your AI agent a sandbox its tool calls can actually run in.",
};

/**
 * Root layout — kept minimal so page-level components own their own
 * chrome. Just wires the tailwind stylesheet + a system font stack.
 */
export default function RootLayout({ children }: { children: ReactNode }) {
  return (
    <html lang="en" className="antialiased">
      <body className="min-h-screen font-sans">{children}</body>
    </html>
  );
}
