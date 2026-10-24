import { contributors } from '@/lib/contributors';
import { errorResponse } from '@/lib/server';
type Context = { params: Promise<{ path?: string[] }> };
export async function GET(request: Request, context: Context) {
  try { return await contributors(request, (await context.params).path ?? []); } catch (e) { return errorResponse(e); }
}
export const POST = GET;
