-- E2E B-ROTA-7 (SC-8): the same swap could be asked twice while the first was
-- open (two awaiting_peer rows, the colleague asked twice). One open swap per
-- (requester, their date and block, colleague, the colleague's date and
-- block). Duplicates already open are cancelled first, the oldest kept, so
-- the index can be built on a database that has them.
UPDATE staff_swaps SET status = 'cancelled'
 WHERE id IN (
     SELECT id FROM (
         SELECT id, row_number() OVER (
                    PARTITION BY requester_id, requester_date, requester_shift_id,
                                 peer_id, peer_date, peer_shift_id
                    ORDER BY created_at, id) AS n
           FROM staff_swaps
          WHERE status IN ('awaiting_peer', 'pending')
     ) x
      WHERE x.n > 1
 );

CREATE UNIQUE INDEX staff_swaps_one_open
    ON staff_swaps (requester_id, requester_date, requester_shift_id,
                    peer_id, peer_date, peer_shift_id)
 WHERE status IN ('awaiting_peer', 'pending');
