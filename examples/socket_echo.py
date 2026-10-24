import _thread
import socket


def client(port, out):
    connection = socket.create_connection(("127.0.0.1", port))
    connection.sendall(b"ping")
    out.append(connection.recv(4))
    connection.close()


server = socket.socket()
server.bind(("127.0.0.1", 0))
server.listen(1)
server.settimeout(5.0)
port = server.getsockname()[1]
out = []
handle = _thread.start_new_thread(client, (port, out))
connection, address = server.accept()
connection.settimeout(5.0)
data = connection.recv(4)
connection.sendall(b"pong")
connection.close()
server.close()
_thread.join(handle)
print(data, out[0])
