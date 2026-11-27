from collections.abc import Sequence

from fastembed import TextEmbedding

# changes.embedding in schema.sql has this model's dimensions
MODEL = "sentence-transformers/all-MiniLM-L6-v2"


class Embedder:
    """Embeds text with MODEL, downloading it to FASTEMBED_CACHE_PATH on
    first use.
    """

    def __init__(self) -> None:
        self._model = TextEmbedding(MODEL, threads=1)

    def __call__(self, texts: Sequence[str]) -> list[list[float]]:
        return [vector.tolist() for vector in self._model.embed(list(texts))]
