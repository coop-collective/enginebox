import { orgAction, orgView } from "@/lib/identity/org";
import { errorResponse, HttpError } from "@/lib/server";

export const dynamic = "force-dynamic";
type Context = { params: Promise<{ path?: string[] }> };

export async function GET(request: Request, context: Context) {
  try {
    if ((await context.params).path?.length) throw new HttpError(404, "Unknown organization operation.");
    return orgView(request);
  } catch (error) {
    return errorResponse(error);
  }
}

export async function POST(request: Request, context: Context) {
  try {
    return await orgAction(request, (await context.params).path ?? []);
  } catch (error) {
    return errorResponse(error);
  }
}
