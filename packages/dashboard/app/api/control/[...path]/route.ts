import { errorResponse, proxy } from "@/lib/server";

export const dynamic = "force-dynamic";
type Context = { params: Promise<{ path: string[] }> };

export async function GET(request: Request, context: Context) {
  try {
    return await proxy(request, (await context.params).path);
  } catch (error) {
    return errorResponse(error);
  }
}

export async function POST(request: Request, context: Context) {
  try {
    return await proxy(request, (await context.params).path);
  } catch (error) {
    return errorResponse(error);
  }
}
