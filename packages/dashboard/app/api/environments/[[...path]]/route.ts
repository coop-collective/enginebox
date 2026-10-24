import { environmentProxy } from '@/lib/environments';
import { errorResponse } from '@/lib/server';
export const dynamic = 'force-dynamic';
type Context = { params: Promise<{ path?: string[] }> };
async function handle(request: Request, context: Context) {
  try {
    const path = (await context.params).path ?? [];
    return await environmentProxy(request, `v1/environments${path.length ? `/${path.join('/')}` : ''}`);
  } catch (error) { return errorResponse(error); }
}
export const GET = handle;
export const POST = handle;
