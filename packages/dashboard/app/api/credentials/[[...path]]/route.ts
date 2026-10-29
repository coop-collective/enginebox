import { credentials } from "@/lib/credentials";
import { errorResponse } from "@/lib/server";

export const dynamic = "force-dynamic";
type Context = { params: Promise<{ path?: string[] }> };

async function handle(request: Request, context: Context) {
  try {
    return await credentials(request, (await context.params).path ?? []);
  } catch (error) {
    return errorResponse(error);
  }
}
export const GET = handle;
export const POST = handle;
