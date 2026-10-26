import { authorize, authorizePost } from "@/lib/oauth/server";
import { errorPage } from "@/lib/oauth/pages";
import { HttpError } from "@/lib/server";

export const dynamic = "force-dynamic";

function failure(error: unknown) {
  if (error instanceof HttpError && error.status === 404) return new Response("Not found", { status: 404 });
  return errorPage("Engine Box could not process this authorization request.", 500);
}

export async function GET(request: Request) {
  try {
    return await authorize(request);
  } catch (error) {
    return failure(error);
  }
}

export async function POST(request: Request) {
  try {
    return await authorizePost(request);
  } catch (error) {
    return failure(error);
  }
}
