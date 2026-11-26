FROM python:3.14.8-slim-trixie
COPY --from=ghcr.io/astral-sh/uv:0.13.6 /uv /bin/uv
ENV UV_COMPILE_BYTECODE=1 UV_LINK_MODE=copy FASTEMBED_CACHE_PATH=/app/models
WORKDIR /app
COPY pyproject.toml uv.lock ./
RUN uv sync --locked --no-dev --no-install-project
COPY README.md ./
COPY src ./src
RUN uv sync --locked --no-dev
RUN /app/.venv/bin/python -c 'from two_tdb.embedding import Embedder; Embedder()'
USER 1000
EXPOSE 8000
CMD ["/app/.venv/bin/uvicorn", "--factory", "two_tdb.api:from_environment", "--host", "0.0.0.0", "--port", "8000"]
