"""Rule interface — the Python port of novel-downloader's BaseRuleClass.

Each site is one Rule subclass with two methods:

  book_parse(url, session)      -> Book (metadata + full chapter list)
  chapter_parse(chapter, session) -> ChapterResult (text or a locked/OCR marker)

Rules are dispatched by URL through `rule_for()`.
"""

from abc import ABC, abstractmethod
from dataclasses import dataclass, field


@dataclass
class Chapter:
    num: int
    title: str
    url: str
    is_vip: bool = False
    # True = you are entitled (限免 / already purchased); False = locked; None = unknown
    is_paid: bool | None = None
    section_name: str | None = None
    section_number: int = 0
    word_count: int = 0


@dataclass
class Book:
    site: str
    url: str
    title: str
    author: str = ""
    intro: str = ""
    tags: list[str] = field(default_factory=list)
    cover: str | None = None
    chapters: list[Chapter] = field(default_factory=list)

    @property
    def vip_chapters(self) -> list[Chapter]:
        return [c for c in self.chapters if c.is_vip]


@dataclass
class ChapterResult:
    title: str
    text: str = ""
    locked: bool = False
    needs_ocr: bool = False
    images: list[str] = field(default_factory=list)


class Rule(ABC):
    name: str = ""
    domains: tuple[str, ...] = ()
    # Serial downloads by default: sites rate-limit aggressive parallel fetching.
    concurrency_limit: int = 1

    @classmethod
    def matches(cls, url: str) -> bool:
        return any(d in url for d in cls.domains)

    @abstractmethod
    def book_parse(self, url: str, session) -> Book:
        ...

    @abstractmethod
    def chapter_parse(self, chapter: Chapter, session) -> ChapterResult:
        ...


_RULES: list[type[Rule]] = []


def register(cls: type[Rule]) -> type[Rule]:
    _RULES.append(cls)
    return cls


def _load_rules() -> list[type[Rule]]:
    """Import the site modules for their registration side effect."""
    if not _RULES:
        from . import qidian, sfacg  # noqa: F401
    return _RULES


def rule_for(url: str) -> Rule:
    for cls in _load_rules():
        if cls.matches(url):
            return cls()
    raise ValueError(f"No rule for {url!r}; known: {known_sites()}")


def known_sites() -> list[str]:
    return [c.name for c in _load_rules()]