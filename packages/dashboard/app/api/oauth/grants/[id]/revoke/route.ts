import { revokeGrantByOwner } from "@/lib/oauth/server";
import { errorResponse } from "@/lib/server";

export const dynamic = "force-dynamic";
type Context = { params: Promise<{ id: string }> };

export async function POST(request: Request, context: Context) {
  try {
    return await revokeGrantByOwner(request, (await context.params).id);
  } catch (error) {
    return errorResponse(error);
  }
}
