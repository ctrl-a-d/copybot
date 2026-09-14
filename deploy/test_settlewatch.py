"""The legacy helper must never race the engine's settlement writer."""
import settlewatch


def test_apply_refuses_before_reading_or_writing(monkeypatch, tmp_path):
    journal = tmp_path / 'ledger.jsonl'
    journal.write_text('preserved\n')
    monkeypatch.setattr(settlewatch, 'LEDGER', str(journal))
    def unexpected():
        raise AssertionError('apply must refuse before scanning')
    monkeypatch.setattr(settlewatch, 'released_phantoms', unexpected)
    assert settlewatch.run(dry_run=False) == 2
    assert journal.read_text() == 'preserved\n'


def test_direct_helper_cannot_append(monkeypatch, tmp_path):
    journal = tmp_path / 'ledger.jsonl'
    journal.write_text('preserved\n')
    monkeypatch.setattr(settlewatch, 'LEDGER', str(journal))
    assert not settlewatch.append_adjustment('a', '101', 10, 1, 0.6, 'synthetic', False)
    assert settlewatch.append_adjustment('a', '101', 10, 1, 0.6, 'synthetic', True)
    assert journal.read_text() == 'preserved\n'
