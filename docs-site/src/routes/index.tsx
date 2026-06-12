import { createFileRoute, Link } from '@tanstack/react-router'
import { HomeLayout } from 'fumadocs-ui/layouts/home'
import { baseOptions } from '@/lib/layout.shared'

export const Route = createFileRoute('/')({ component: Home })

function Home() {
  return (
    <HomeLayout {...baseOptions()}>
      <main className="flex flex-1 flex-col items-center justify-center gap-6 px-4 py-24 text-center">
        <h1 className="text-4xl font-bold tracking-tight">punktfunk</h1>
        <p className="max-w-xl text-fd-muted-foreground">
          A ground-up low-latency desktop and game streaming stack, built Linux-first, with a
          shared Rust protocol core and native clients per platform.
        </p>
        <Link
          to="/docs/$"
          params={{ _splat: '' }}
          className="rounded-lg bg-fd-primary px-5 py-2.5 font-medium text-fd-primary-foreground"
        >
          Read the docs
        </Link>
      </main>
    </HomeLayout>
  )
}
