import { readFileSync } from "node:fs";
import path from "node:path";
import type { Metadata } from "next";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeSlug from "rehype-slug";
import { Nav } from "../components/Nav";
import { Footer } from "../components/Footer";
import { DocsSidebar } from "../components/DocsSidebar";
import { mdComponents } from "../components/mdComponents";

export const metadata: Metadata = {
  title: "文档 — smelt",
  description: "smelt 使用文档：快速开始、核心概念、功能一览、配置文件。",
};

export default function DocsPage() {
  const filePath = path.join(process.cwd(), "content", "docs.md");
  const source = readFileSync(filePath, "utf-8");

  return (
    <div className="flex flex-1 flex-col bg-background">
      <Nav />
      <main className="mx-auto flex w-full max-w-4xl flex-1 gap-12 px-6 py-16">
        <DocsSidebar />
        <article className="min-w-0 flex-1">
          <ReactMarkdown
            remarkPlugins={[remarkGfm]}
            rehypePlugins={[rehypeSlug]}
            components={mdComponents}
          >
            {source}
          </ReactMarkdown>
        </article>
      </main>
      <Footer />
    </div>
  );
}
