import { cn } from '@/lib/utils'
import { BrandMark } from './brand-mark'
import { Wordmark } from './wordmark'

// Full punktfunk lockup: the lens mark anchored to the top-left corner of the
// "funk" wordmark. Size the lockup with a width on the wrapper (e.g. `w-40`);
// the mark scales as a fraction of that width.
export function Logo({ className }: { className?: string }) {
  return (
    <div className={cn('relative inline-block', className)}>
      <BrandMark className="absolute left-0 top-0 w-[24%] -translate-x-[55%] -translate-y-[58%] drop-shadow-[0_4px_24px_rgba(108,91,243,0.45)]" />
      <Wordmark className="block h-auto w-full" />
    </div>
  )
}

export default Logo
