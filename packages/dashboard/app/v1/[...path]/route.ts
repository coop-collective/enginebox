import { resourceProxy } from "@/lib/oauth/server";
import { errorResponse } from "@/lib/server";

export const dynamic = "force-dynamic";
type Context = { params: Promise<{ path: string[] }> };

async function handle(request: Request, context: Context) {
  try {
    return await resourceProxy(request, ["v1", ...(await context.params).path]);
  } catch (error) {
    return errorResponse(error);
  }
}
export const GET = handle;
export const POST = handle;
