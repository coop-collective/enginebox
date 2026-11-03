import { enginePeerProxy } from '@/lib/contributors';
import { errorResponse } from '@/lib/server';

export const dynamic = 'force-dynamic';
type Context = { params: Promise<{ path: string[] }> };

async function handle(request: Request, context: Context) {
  try { return await enginePeerProxy(request, (await context.params).path); } catch (e) { return errorResponse(e); }
}
export const GET = handle;
export const POST = handle;
