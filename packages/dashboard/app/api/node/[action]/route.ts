import { nodeProxy } from '@/lib/contributors';
import { errorResponse } from '@/lib/server';
export async function POST(request: Request, context: { params: Promise<{ action: string }> }) {
  try { return await nodeProxy(request, (await context.params).action); } catch (e) { return errorResponse(e); }
}
