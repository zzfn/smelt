import { Nav } from "./components/Nav";
import { Hero } from "./components/Hero";
import { Integrations } from "./components/Integrations";
import { Features } from "./components/Features";
import { FeatureComparison } from "./components/FeatureComparison";
import { CodeSnippet } from "./components/CodeSnippet";
import { DocsCTA } from "./components/DocsCTA";
import { Footer } from "./components/Footer";

export default function Home() {
  return (
    <div className="flex flex-1 flex-col bg-background">
      <Nav />
      <main className="flex-1">
        <Hero />
        <Integrations />
        <Features />
        <FeatureComparison />
        <CodeSnippet />
        <DocsCTA />
      </main>
      <Footer />
    </div>
  );
}
